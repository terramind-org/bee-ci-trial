//! `IssueService`: issues, labels, milestones, assignees (agents included), types, fields,
//! sub-issues, dependencies, templates, pinning, locking, transfer, timeline, development
//! panel, task lists and saved replies. Authorization levels and their meaning are in
//! `docs/spec/forge-work.md` §3.

use bee_policy::Action;
use bee_proto::v1::issue_service_server::IssueService;
use bee_proto::v1::*;
use bee_search::forge::{Filter, FilterContext, Predicate, parse_forge_query};
use bee_search::{ForgeKind, QueryError};
use tonic::{Request, Response, Status};

use super::work::Level;
use super::{ForgeServices, next, page, reply};
use crate::auth::Caller;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::work::{
    ChangeLink, IssueListQuery, IssueTarget, ListSort, NewIssue, StateSelector, TextEffects,
    default_branch_name,
};

fn page_size(p: &Option<PageRequest>) -> (String, usize) {
    page(p)
}

fn query_error(errs: Vec<QueryError>) -> ForgeError {
    let first = errs.first();
    ForgeError::invalid(
        first.map(|e| e.subcode).unwrap_or("bad_query"),
        first
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "the query does not parse".into()),
    )
}

fn walk(f: &Filter, visit: &mut dyn FnMut(&Predicate)) {
    match f {
        Filter::Pred(p) => visit(p),
        Filter::Not(i) => walk(i, visit),
        Filter::And(v) | Filter::Or(v) => v.iter().for_each(|c| walk(c, visit)),
        Filter::All => {}
    }
}

fn day_of(ms: u64) -> bee_search::forge::CivilDate {
    bee_search::forge::CivilDate::from_days((ms / 86_400_000) as i64)
}

fn state_reason(v: i32) -> IssueStateReason {
    IssueStateReason::try_from(v).unwrap_or(IssueStateReason::Unspecified)
}

impl ForgeServices {
    /// Keeps the summaries the caller may read. A sub-issue may live in another repo of the
    /// org (a private one under a public parent): those the caller cannot `Read` are left out,
    /// so listing a public issue never shows a private repo's issue titles. Call after
    /// [`Self::fix_names`], which gives every summary its repo's name.
    async fn readable_summaries(
        &self,
        caller: &Caller,
        home: &Repo,
        items: Vec<IssueSummary>,
    ) -> Vec<IssueSummary> {
        let mut readable: std::collections::BTreeMap<String, bool> = Default::default();
        readable.insert(home.full_name.clone(), true);
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let ok = match readable.get(&it.repo_full_name) {
                Some(ok) => *ok,
                None => {
                    let r = Some(RepoRef {
                        repo: Some(repo_ref::Repo::FullName(it.repo_full_name.clone())),
                    });
                    let ok = match self.repo(caller, &r).await {
                        Ok(target) => self
                            .forge
                            .authz
                            .repo(caller, &target, Action::Read, &[], None, None)
                            .await
                            .is_ok(),
                        Err(_) => false,
                    };
                    readable.insert(it.repo_full_name.clone(), ok);
                    ok
                }
            };
            if ok {
                out.push(it);
            }
        }
        out
    }

    /// Sets `inspired_by` on issues the caller is served: the post that inspired each, when
    /// the caller reads the post's repo (`crate::posts::service::fill_issue_inspirations`).
    async fn inspire(
        &self,
        caller: &Caller,
        cx: &crate::cx::Cx,
        issues: &mut [Issue],
    ) -> Result<()> {
        crate::posts::service::fill_issue_inspirations(&self.forge, caller, cx, issues).await
    }

    /// Resolves an issue reference to a target and checks the caller may act on it with
    /// `action`; returns the target's repo.
    async fn issue_ref_target(
        &self,
        caller: &Caller,
        r: &Option<IssueRef>,
        action: Action,
    ) -> Result<(IssueTarget, Repo)> {
        let r = r.as_ref().ok_or_else(|| ForgeError::missing("issue"))?;
        let (target, repo_id) = if !r.id.is_empty() {
            let repo_id = self.forge.work.issue_repo(&caller.cx, &r.id).await?;
            (IssueTarget::Id(r.id.clone()), repo_id)
        } else {
            let repo = self.repo(caller, &r.repo).await?;
            if r.number == 0 {
                return Err(ForgeError::missing("issue.number"));
            }
            (
                IssueTarget::Number {
                    repo_id: repo.id.clone(),
                    number: r.number,
                },
                repo.id,
            )
        };
        let repo = self
            .forge
            .orgs
            .resolve_repo(
                &caller.cx,
                &RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo_id)),
                },
            )
            .await?;
        self.forge
            .authz
            .repo(caller, &repo, action, &[], None, None)
            .await?;
        Ok((target, repo))
    }

    /// The repo an issue lives in, with `Read` checked (`NOT_FOUND` when unreadable).
    async fn issue_repo_readable(&self, caller: &Caller, repo_id: &str) -> Result<Repo> {
        let repo = self
            .forge
            .orgs
            .resolve_repo(
                &caller.cx,
                &RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo_id.to_string())),
                },
            )
            .await?;
        self.forge
            .authz
            .repo(caller, &repo, Action::Read, &[], None, None)
            .await?;
        Ok(repo)
    }

    async fn list_query(
        &self,
        caller: &Caller,
        repo: &Repo,
        r: &ListIssuesRequest,
    ) -> Result<IssueListQuery> {
        let me = self.login_of(&caller.cx.actor_id).await;
        let now = self.forge.db.now_ms();
        let mut q = IssueListQuery {
            state: if r.all_states {
                StateSelector::All
            } else if r.state == IssueState::Closed as i32 {
                StateSelector::Closed
            } else {
                StateSelector::Open
            },
            labels: r.labels.clone(),
            milestone: r.milestone.clone(),
            issue_type: r.issue_type.clone(),
            since_ms: r.since.as_ref().map(ts_ms),
            sort: match IssueSort::try_from(r.sort).unwrap_or(IssueSort::Unspecified) {
                IssueSort::Updated => ListSort::Updated,
                IssueSort::Comments => ListSort::Comments,
                IssueSort::Reactions => ListSort::Reactions,
                _ => ListSort::Created,
            },
            ascending: r.ascending,
            visibility: match Visibility::try_from(repo.visibility)
                .unwrap_or(Visibility::Unspecified)
            {
                Visibility::Public => "public",
                Visibility::Internal => "internal",
                _ => "private",
            }
            .into(),
            archived: repo.archived,
            ..Default::default()
        };
        // Request fields name principals by id or handle.
        for (field, slot) in [(&r.assignee, 0), (&r.author, 1), (&r.mentioned, 2)] {
            if field.is_empty() {
                continue;
            }
            let id = if slot == 0 && (field == "none" || field == "*") {
                field.clone()
            } else {
                self.principal_of(field).await?.id
            };
            match slot {
                0 => q.assignee = id,
                1 => q.author = id,
                _ => q.mentioned = id,
            }
        }
        if !r.query.trim().is_empty() {
            let parsed = parse_forge_query(ForgeKind::Issues, &r.query).map_err(query_error)?;
            let compiled = parsed
                .to_filter(&FilterContext {
                    me: Some(me),
                    today: Some(day_of(now)),
                })
                .map_err(|e| ForgeError::invalid("bad_query", e.message))?;
            let mut logins: Vec<String> = Vec::new();
            walk(&compiled.filter, &mut |p| match p {
                Predicate::Person { login, .. } => logins.push(login.clone()),
                Predicate::AgentAuthor { login: Some(l), .. } => logins.push(l.clone()),
                _ => {}
            });
            for l in logins {
                let key = l.to_lowercase();
                if let Ok(p) = self.principal_of(&l).await {
                    q.people.insert(key, p.id);
                }
            }
            q.query_sort = Some(compiled.sort);
            q.filter = Some(compiled.filter);
        }
        Ok(q)
    }

    async fn template_defaults(
        &self,
        repo: &Repo,
        r: &CreateIssueRequest,
    ) -> Result<Option<IssueTemplate>> {
        if r.template.is_empty() {
            if !r.form_responses.is_empty() {
                return Err(ForgeError::invalid(
                    "template_required",
                    "`form_responses` needs a `template`",
                ));
            }
            return Ok(None);
        }
        Ok(Some(
            self.forge.work.find_template(repo, &r.template).await?,
        ))
    }
}

#[tonic::async_trait]
impl IssueService for ForgeServices {
    // ---------------- issues ----------------

    async fn create_issue(
        &self,
        req: Request<CreateIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self.issue_access(&caller, &repo, Level::Read, "issue.create").await?;
            self.forge
                .guard_write(&cx, &repo, crate::hooks::GuardedWrite::Issue, None)
                .await?;
            let tpl = self.template_defaults(&repo, &r).await?;
            // Planning fields from the request need triage; a template's defaults do not.
            let planning = !r.assignees.is_empty()
                || !r.labels.is_empty()
                || r.milestone_number != 0
                || !r.issue_type.is_empty()
                || r.parent.is_some()
                || !r.field_values.is_empty()
                || !r.project_ids.is_empty();
            if planning && !actor.triage {
                return Err(ForgeError::permission_denied(
                    "setting assignees, labels, a milestone, a type, a parent, fields or projects needs triage access",
                ));
            }
            let mut title = r.title.clone();
            let mut body = r.body.clone();
            let mut labels = r.labels.clone();
            let mut handles = r.assignees.clone();
            let mut issue_type = r.issue_type.clone();
            if let Some(t) = &tpl {
                if title.trim().is_empty() {
                    title = t.title.clone();
                }
                if t.is_form {
                    body = crate::work::render_form_body(&t.form_fields, &r.form_responses)?;
                } else if body.trim().is_empty() {
                    body = t.body.clone();
                }
                if !t.labels.is_empty() {
                    self.forge.work.seed_default_labels(&cx, &Self::info(&repo)).await?;
                }
                for l in &t.labels {
                    if self.forge.work.get_label(&cx, &Self::info(&repo), l).await.is_ok() && !labels.contains(l) {
                        labels.push(l.clone());
                    }
                }
                let ok = self.assignable(&cx, &repo).await?;
                for a in &t.assignees {
                    if let Ok(p) = self.principal_of(a).await
                        && ok.iter().any(|x| x.id == p.id)
                        && !handles.contains(&p.id)
                    {
                        handles.push(p.id);
                    }
                }
                if issue_type.is_empty() {
                    issue_type = t.issue_type.clone();
                }
            }
            if !labels.is_empty() {
                self.forge.work.seed_default_labels(&cx, &Self::info(&repo)).await?;
            }
            let assignees = self.assignees_of(&cx, &repo, &handles).await?;
            let agent_runs = self.agent_runs(&repo, &assignees, "", "").await?;
            let parent = match &r.parent {
                Some(p) => {
                    let (t, _) = self.issue_ref_target(&caller, &Some(p.clone()), Action::Triage).await?;
                    Some(t)
                }
                None => None,
            };
            let effects = self.effects_of(&caller, &repo, &body).await;
            for p in &r.project_ids {
                self.project_write(&caller, p, "project_item.add").await?;
            }
            // The post that inspired the issue: a published post the caller can read.
            let inspired_by = if r.inspired_by_post_id.trim().is_empty() {
                None
            } else {
                let posts = self.forge.work.posts().ok_or_else(|| {
                    super::unserved(
                        "CreateIssue.inspired_by_post_id",
                        "the posts module (crate::posts::Posts::attach)",
                    )
                })?;
                Some(
                    posts
                        .inspiration(&self.forge.orgs, &cx, &r.inspired_by_post_id)
                        .await?,
                )
            };
            let mut issue = self
                .forge
                .work
                .create_issue(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    NewIssue {
                        title,
                        body,
                        assignees,
                        agent_runs,
                        labels,
                        milestone_number: r.milestone_number,
                        issue_type,
                        parent,
                        field_values: r.field_values.clone(),
                        template: r.template.clone(),
                        effects,
                        project_ids: r.project_ids.clone(),
                        inspired_by,
                    },
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue)).await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_issue(&self, req: Request<GetIssueRequest>) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo_id = self.forge.work.issue_repo(&caller.cx, &r.id).await?;
            let repo = self.issue_repo_readable(&caller, &repo_id).await?;
            let mut issue = self
                .forge
                .work
                .get_issue(&caller.cx, &Self::info(&repo), &r.id)
                .await?;
            self.fix_one(&caller.cx, &mut issue).await;
            self.inspire(&caller, &caller.cx, std::slice::from_mut(&mut issue))
                .await?;
            if r.include_html {
                issue.body_html = crate::markdown::render_html(&issue.body);
            }
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_issue_by_number(
        &self,
        req: Request<GetIssueByNumberRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            self.forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let mut issue = self
                .forge
                .work
                .get_issue_by_number(&caller.cx, &Self::info(&repo), r.number)
                .await?;
            if issue.repo_id != repo.id {
                // Served from the issue's new home: the caller must read it there too.
                let home = self.issue_repo_readable(&caller, &issue.repo_id).await?;
                issue.repo_full_name = home.full_name;
            }
            self.fix_one(&caller.cx, &mut issue).await;
            self.inspire(&caller, &caller.cx, std::slice::from_mut(&mut issue))
                .await?;
            if r.include_html {
                issue.body_html = crate::markdown::render_html(&issue.body);
            }
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issues(
        &self,
        req: Request<ListIssuesRequest>,
    ) -> Result<Response<ListIssuesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.list")
                .await?;
            let q = self.list_query(&caller, &repo, &r).await?;
            let (token, size) = page_size(&r.page);
            let mut res = self
                .forge
                .work
                .list_issues(&cx, &Self::info(&repo), &q, size, &token)
                .await?;
            self.fix_names(&cx, &mut res.issues).await;
            self.inspire(&caller, &cx, &mut res.issues).await?;
            Ok(ListIssuesResponse {
                issues: res.issues,
                total_count: res.total,
                page: next(res.next),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_issue(
        &self,
        req: Request<UpdateIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Read, "issue.update")
                .await?;
            let effects = match &r.body {
                Some(b) => self.effects_of(&caller, &repo, b).await,
                None => TextEffects::default(),
            };
            let mut issue = self
                .forge
                .work
                .update_issue(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    r.title,
                    r.body,
                    r.expected_version,
                    &effects,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_issue(
        &self,
        req: Request<DeleteIssueRequest>,
    ) -> Result<Response<DeleteIssueResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Admin, "issue.delete")
                .await?;
            self.forge
                .work
                .delete_issue(&cx, &Self::info(&repo), &actor, r.number)
                .await?;
            Ok(DeleteIssueResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn close_issue(
        &self,
        req: Request<CloseIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Read, "issue.close")
                .await?;
            let dup = match &r.duplicate_of {
                Some(d) => Some(
                    self.issue_ref_target(&caller, &Some(d.clone()), Action::Read)
                        .await?
                        .0,
                ),
                None => None,
            };
            let effects = self.effects_of(&caller, &repo, &r.comment).await;
            let mut issue = self
                .forge
                .work
                .close_issue(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    state_reason(r.reason),
                    dup,
                    &r.comment,
                    r.expected_version,
                    &effects,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn reopen_issue(
        &self,
        req: Request<ReopenIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Read, "issue.reopen")
                .await?;
            let effects = self.effects_of(&caller, &repo, &r.comment).await;
            let mut issue = self
                .forge
                .work
                .reopen_issue(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    &r.comment,
                    r.expected_version,
                    &effects,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn mark_issue_duplicate(
        &self,
        req: Request<MarkIssueDuplicateRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Read, "issue.mark_duplicate")
                .await?;
            let (target, _) = self
                .issue_ref_target(&caller, &r.duplicate_of, Action::Read)
                .await?;
            let mut issue = self
                .forge
                .work
                .mark_duplicate(&cx, &Self::info(&repo), &actor, r.number, target)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn unmark_issue_duplicate(
        &self,
        req: Request<UnmarkIssueDuplicateRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Read, "issue.unmark_duplicate")
                .await?;
            let mut issue = self
                .forge
                .work
                .unmark_duplicate(&cx, &Self::info(&repo), &actor, r.number, r.reopen)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- comments ----------------

    async fn create_issue_comment(
        &self,
        req: Request<CreateIssueCommentRequest>,
    ) -> Result<Response<Comment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Read, "issue_comment.create")
                .await?;
            self.forge
                .guard_write(
                    &cx,
                    &repo,
                    crate::hooks::GuardedWrite::Comment,
                    Some(bee_proto::v1::SubjectRef {
                        kind: bee_proto::v1::SubjectKind::Issue as i32,
                        repo: None,
                        id: r.number.to_string(),
                    }),
                )
                .await?;
            let effects = self.effects_of(&caller, &repo, &r.body).await;
            self.forge
                .work
                .create_comment(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    &r.body,
                    &r.saved_reply_id,
                    r.close,
                    r.reopen,
                    &effects,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_comments(
        &self,
        req: Request<ListIssueCommentsRequest>,
    ) -> Result<Response<ListIssueCommentsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue_comment.list")
                .await?;
            let (token, size) = page_size(&r.page);
            let (mut comments, tok) = self
                .forge
                .work
                .list_comments(
                    &cx,
                    &Self::info(&repo),
                    r.number,
                    r.since.as_ref().map(ts_ms),
                    size,
                    &token,
                )
                .await?;
            if r.include_html {
                for c in &mut comments {
                    c.body_html = crate::markdown::render_html(&c.body);
                }
            }
            Ok(ListIssueCommentsResponse {
                comments,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- labels ----------------

    async fn create_issue_label(
        &self,
        req: Request<CreateIssueLabelRequest>,
    ) -> Result<Response<Label>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "label.create")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            self.forge
                .work
                .create_label(&cx, &info, &actor, &r.name, &r.color, &r.description)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_issue_label(
        &self,
        req: Request<GetIssueLabelRequest>,
    ) -> Result<Response<Label>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "label.get")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            self.forge.work.get_label(&cx, &info, &r.name).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_issue_label(
        &self,
        req: Request<UpdateIssueLabelRequest>,
    ) -> Result<Response<Label>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::Triage, "label.update")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            self.forge
                .work
                .update_label(&cx, &info, &r.name, r.new_name, r.color, r.description)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_issue_label(
        &self,
        req: Request<DeleteIssueLabelRequest>,
    ) -> Result<Response<DeleteIssueLabelResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::Triage, "label.delete")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            self.forge.work.delete_label(&cx, &info, &r.name).await?;
            Ok(DeleteIssueLabelResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_labels(
        &self,
        req: Request<ListIssueLabelsRequest>,
    ) -> Result<Response<ListIssueLabelsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "label.list")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            let (token, size) = page_size(&r.page);
            let (labels, tok) = self
                .forge
                .work
                .list_labels(&cx, &info, &r.query, size, &token)
                .await?;
            Ok(ListIssueLabelsResponse {
                labels,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_issue_labels(
        &self,
        req: Request<AddIssueLabelsRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.label")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            let mut issue = self
                .forge
                .work
                .add_issue_labels(&cx, &info, &actor, r.number, &r.labels, r.create_missing)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_issue_labels(
        &self,
        req: Request<RemoveIssueLabelsRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.label")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            let mut issue = self
                .forge
                .work
                .remove_issue_labels(&cx, &info, &actor, r.number, &r.labels)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn set_issue_labels(
        &self,
        req: Request<SetIssueLabelsRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.label")
                .await?;
            let info = Self::info(&repo);
            self.forge.work.seed_default_labels(&cx, &info).await?;
            let mut issue = self
                .forge
                .work
                .set_issue_labels(&cx, &info, &actor, r.number, &r.labels, r.expected_version)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- milestones ----------------

    async fn create_issue_milestone(
        &self,
        req: Request<CreateIssueMilestoneRequest>,
    ) -> Result<Response<Milestone>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::Triage, "milestone.create")
                .await?;
            self.forge
                .work
                .create_milestone(
                    &cx,
                    &Self::info(&repo),
                    &r.title,
                    &r.description,
                    r.due_on.as_ref().map(ts_ms),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_issue_milestone(
        &self,
        req: Request<GetIssueMilestoneRequest>,
    ) -> Result<Response<Milestone>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "milestone.get")
                .await?;
            self.forge
                .work
                .get_milestone(&cx, &Self::info(&repo), r.number)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_issue_milestone(
        &self,
        req: Request<UpdateIssueMilestoneRequest>,
    ) -> Result<Response<Milestone>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::Triage, "milestone.update")
                .await?;
            self.forge
                .work
                .update_milestone(
                    &cx,
                    &Self::info(&repo),
                    r.number,
                    r.title,
                    r.description,
                    r.due_on.as_ref().map(ts_ms),
                    r.clear_due_on,
                    MilestoneState::try_from(r.state).unwrap_or(MilestoneState::Unspecified),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_issue_milestone(
        &self,
        req: Request<DeleteIssueMilestoneRequest>,
    ) -> Result<Response<DeleteIssueMilestoneResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::Triage, "milestone.delete")
                .await?;
            self.forge
                .work
                .delete_milestone(&cx, &Self::info(&repo), r.number)
                .await?;
            Ok(DeleteIssueMilestoneResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_milestones(
        &self,
        req: Request<ListIssueMilestonesRequest>,
    ) -> Result<Response<ListIssueMilestonesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "milestone.list")
                .await?;
            let state = if r.all_states {
                None
            } else if r.state == MilestoneState::Closed as i32 {
                Some(MilestoneState::Closed)
            } else {
                Some(MilestoneState::Open)
            };
            let (token, size) = page_size(&r.page);
            let (milestones, tok) = self
                .forge
                .work
                .list_milestones(&cx, &Self::info(&repo), state, size, &token)
                .await?;
            Ok(ListIssueMilestonesResponse {
                milestones,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_issue_milestone(
        &self,
        req: Request<SetIssueMilestoneRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.milestone")
                .await?;
            let mut issue = self
                .forge
                .work
                .set_issue_milestone(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    r.milestone_number,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- assignees ----------------

    async fn add_issue_assignees(
        &self,
        req: Request<AddIssueAssigneesRequest>,
    ) -> Result<Response<AddIssueAssigneesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.assign")
                .await?;
            let principals = self.assignees_of(&cx, &repo, &r.assignees).await?;
            let base_ref = if r.base_ref.is_empty() {
                repo.default_ref.clone()
            } else {
                r.base_ref.clone()
            };
            // A funding failure surfaces before anything changes.
            let runs = self
                .agent_runs(&repo, &principals, &r.agent_instructions, &base_ref)
                .await?;
            let (mut issue, started) = self
                .forge
                .work
                .add_assignees(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    &principals,
                    &runs,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(AddIssueAssigneesResponse {
                issue: Some(issue),
                agent_runs: started
                    .into_iter()
                    .map(|(agent, run)| IssueAgentRunStart {
                        agent: Some(agent),
                        agent_run_id: run,
                    })
                    .collect(),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_issue_assignees(
        &self,
        req: Request<RemoveIssueAssigneesRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.unassign")
                .await?;
            let mut ids = Vec::new();
            for h in &r.assignees {
                match self.principal_of(h).await {
                    Ok(p) => ids.push(p.id),
                    Err(_) => ids.push(h.clone()),
                }
            }
            let mut issue = self
                .forge
                .work
                .remove_assignees(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    &ids,
                    r.stop_agent_runs,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_assignable_users(
        &self,
        req: Request<ListIssueAssignableUsersRequest>,
    ) -> Result<Response<ListIssueAssignableUsersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.list_assignable")
                .await?;
            let q = r.query.to_lowercase();
            let mut all = self.assignable(&cx, &repo).await?;
            let mut keep = Vec::new();
            for p in all.drain(..) {
                if r.kind != PrincipalKind::Unspecified as i32 && p.kind != r.kind {
                    continue;
                }
                let (logins, _) = self.forge.hooks.directory.identities(&p.id).await;
                let hit = q.is_empty()
                    || p.display.to_lowercase().starts_with(&q)
                    || p.id.to_lowercase().starts_with(&q)
                    || logins.iter().any(|l| l.to_lowercase().starts_with(&q));
                if hit {
                    keep.push(p);
                }
            }
            keep.sort_by(|a, b| a.id.cmp(&b.id));
            let (token, size) = page_size(&r.page);
            let rest: Vec<Principal> = keep
                .into_iter()
                .filter(|p| token.is_empty() || p.id > token)
                .collect();
            let tok = if rest.len() > size {
                rest[size - 1].id.clone()
            } else {
                String::new()
            };
            Ok(ListIssueAssignableUsersResponse {
                principals: rest.into_iter().take(size).collect(),
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- issue types ----------------

    async fn create_issue_type(
        &self,
        req: Request<CreateIssueTypeRequest>,
    ) -> Result<Response<IssueType>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self
                .org_manage(&caller, &r.org, "issue_type.create")
                .await?;
            self.forge
                .work
                .create_issue_type(&cx, &org_id, &r.name, &r.description, &r.color, r.enabled)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_issue_type(
        &self,
        req: Request<UpdateIssueTypeRequest>,
    ) -> Result<Response<IssueType>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self
                .org_manage(&caller, &r.org, "issue_type.update")
                .await?;
            self.forge
                .work
                .update_issue_type(
                    &cx,
                    &org_id,
                    &r.id,
                    r.name,
                    r.description,
                    r.color,
                    r.enabled,
                    r.position,
                    r.expected_version,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_issue_type(
        &self,
        req: Request<DeleteIssueTypeRequest>,
    ) -> Result<Response<DeleteIssueTypeResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self
                .org_manage(&caller, &r.org, "issue_type.delete")
                .await?;
            self.forge
                .work
                .delete_issue_type(&cx, &org_id, &r.id)
                .await?;
            Ok(DeleteIssueTypeResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_types(
        &self,
        req: Request<ListIssueTypesRequest>,
    ) -> Result<Response<ListIssueTypesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self.org_or_repo_view(&caller, &r.org, &r.repo).await?;
            let (token, size) = page_size(&r.page);
            let (issue_types, tok) = self
                .forge
                .work
                .list_issue_types(&cx, &org_id, r.include_disabled, size, &token)
                .await?;
            Ok(ListIssueTypesResponse {
                issue_types,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_issue_type(
        &self,
        req: Request<SetIssueTypeRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.set_type")
                .await?;
            let mut issue = self
                .forge
                .work
                .set_issue_type(&cx, &Self::info(&repo), &actor, r.number, &r.issue_type)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- sub-issues ----------------

    async fn add_issue_sub_issue(
        &self,
        req: Request<AddIssueSubIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.add_sub_issue")
                .await?;
            let (child, _) = self
                .issue_ref_target(&caller, &r.sub_issue, Action::Triage)
                .await?;
            let mut issue = self
                .forge
                .work
                .add_sub_issue(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    child,
                    r.replace_parent,
                    &r.after_id,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_issue_sub_issue(
        &self,
        req: Request<RemoveIssueSubIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.remove_sub_issue")
                .await?;
            let (child, _) = self
                .issue_ref_target(&caller, &r.sub_issue, Action::Read)
                .await?;
            let mut issue = self
                .forge
                .work
                .remove_sub_issue(&cx, &Self::info(&repo), &actor, r.number, child)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn reorder_issue_sub_issue(
        &self,
        req: Request<ReorderIssueSubIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.reorder_sub_issue")
                .await?;
            let (child, _) = self
                .issue_ref_target(&caller, &r.sub_issue, Action::Read)
                .await?;
            let mut issue = self
                .forge
                .work
                .reorder_sub_issue(
                    &cx,
                    &Self::info(&repo),
                    r.number,
                    child,
                    &r.after_id,
                    &r.before_id,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_sub_issues(
        &self,
        req: Request<ListIssueSubIssuesRequest>,
    ) -> Result<Response<ListIssueSubIssuesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.list_sub_issues")
                .await?;
            let (token, size) = page_size(&r.page);
            let (mut sub_issues, progress, tok) = self
                .forge
                .work
                .list_sub_issues(&cx, &Self::info(&repo), r.number, size, &token)
                .await?;
            self.fix_names(&cx, &mut sub_issues).await;
            let sub_issues = self.readable_summaries(&caller, &repo, sub_issues).await;
            Ok(ListIssueSubIssuesResponse {
                sub_issues,
                progress: Some(progress),
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_issue_parent(
        &self,
        req: Request<GetIssueParentRequest>,
    ) -> Result<Response<GetIssueParentResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.get_parent")
                .await?;
            let mut parent = self
                .forge
                .work
                .get_parent(&cx, &Self::info(&repo), r.number)
                .await?;
            if let Some(p) = parent.as_mut() {
                self.fix_one(&cx, p).await;
            }
            Ok(GetIssueParentResponse { parent })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- dependencies ----------------

    async fn add_issue_dependency(
        &self,
        req: Request<AddIssueDependencyRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.add_dependency")
                .await?;
            let (blocker, _) = self
                .issue_ref_target(&caller, &r.blocked_by, Action::Triage)
                .await?;
            let mut issue = self
                .forge
                .work
                .add_dependency(&cx, &Self::info(&repo), &actor, r.number, blocker)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_issue_dependency(
        &self,
        req: Request<RemoveIssueDependencyRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.remove_dependency")
                .await?;
            let (blocker, _) = self
                .issue_ref_target(&caller, &r.blocked_by, Action::Read)
                .await?;
            let mut issue = self
                .forge
                .work
                .remove_dependency(&cx, &Self::info(&repo), &actor, r.number, blocker)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_dependencies(
        &self,
        req: Request<ListIssueDependenciesRequest>,
    ) -> Result<Response<ListIssueDependenciesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.list_dependencies")
                .await?;
            let (token, size) = page_size(&r.page);
            let dir = IssueDependencyDirection::try_from(r.direction)
                .unwrap_or(IssueDependencyDirection::Unspecified);
            let (mut issues, tok) = self
                .forge
                .work
                .list_dependencies(&cx, &Self::info(&repo), r.number, dir, size, &token)
                .await?;
            self.fix_names(&cx, &mut issues).await;
            Ok(ListIssueDependenciesResponse {
                issues,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- custom fields ----------------

    async fn create_issue_field(
        &self,
        req: Request<CreateIssueFieldRequest>,
    ) -> Result<Response<IssueField>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self
                .org_manage(&caller, &r.org, "issue_field.create")
                .await?;
            self.forge
                .work
                .create_issue_field(
                    &cx,
                    &org_id,
                    &r.name,
                    &r.description,
                    IssueFieldKind::try_from(r.kind).unwrap_or(IssueFieldKind::Unspecified),
                    &r.options,
                    &r.iterations,
                    &r.issue_type_ids,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_issue_field(
        &self,
        req: Request<UpdateIssueFieldRequest>,
    ) -> Result<Response<IssueField>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self
                .org_manage(&caller, &r.org, "issue_field.update")
                .await?;
            self.forge
                .work
                .update_issue_field(
                    &cx,
                    &org_id,
                    &r.id,
                    r.name,
                    r.description,
                    r.set_options.then_some(r.options.as_slice()),
                    r.set_iterations.then_some(r.iterations.as_slice()),
                    r.set_issue_type_ids.then_some(r.issue_type_ids.as_slice()),
                    r.expected_version,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_issue_field(
        &self,
        req: Request<DeleteIssueFieldRequest>,
    ) -> Result<Response<DeleteIssueFieldResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self
                .org_manage(&caller, &r.org, "issue_field.delete")
                .await?;
            self.forge
                .work
                .delete_issue_field(&cx, &org_id, &r.id)
                .await?;
            Ok(DeleteIssueFieldResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_issue_fields(
        &self,
        req: Request<ListIssueFieldsRequest>,
    ) -> Result<Response<ListIssueFieldsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org_id) = self.org_or_repo_view(&caller, &r.org, &r.repo).await?;
            let (token, size) = page_size(&r.page);
            let (fields, tok) = self
                .forge
                .work
                .list_issue_fields(&cx, &org_id, size, &token)
                .await?;
            Ok(ListIssueFieldsResponse {
                fields,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_issue_field_values(
        &self,
        req: Request<SetIssueFieldValuesRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.set_field_values")
                .await?;
            let mut issue = self
                .forge
                .work
                .set_issue_field_values(&cx, &Self::info(&repo), &actor, r.number, &r.values)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn clear_issue_field_value(
        &self,
        req: Request<ClearIssueFieldValueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.clear_field_value")
                .await?;
            let mut issue = self
                .forge
                .work
                .clear_issue_field_value(&cx, &Self::info(&repo), &actor, r.number, &r.field_id)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- templates ----------------

    async fn list_issue_templates(
        &self,
        req: Request<ListIssueTemplatesRequest>,
    ) -> Result<Response<ListIssueTemplatesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            self.issue_access(&caller, &repo, Level::View, "issue.list_templates")
                .await?;
            let (templates, revision) = self.forge.work.list_templates(&repo).await?;
            Ok(ListIssueTemplatesResponse {
                templates,
                revision,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_issue_template_config(
        &self,
        req: Request<GetIssueTemplateConfigRequest>,
    ) -> Result<Response<IssueTemplateConfig>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            self.issue_access(&caller, &repo, Level::View, "issue.template_config")
                .await?;
            self.forge.work.template_config(&repo).await
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- pinning and locking ----------------

    async fn pin_issue(&self, req: Request<PinIssueRequest>) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.pin")
                .await?;
            let mut issue = self
                .forge
                .work
                .pin_issue(&cx, &Self::info(&repo), &actor, r.number)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn unpin_issue(
        &self,
        req: Request<UnpinIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.unpin")
                .await?;
            let mut issue = self
                .forge
                .work
                .unpin_issue(&cx, &Self::info(&repo), &actor, r.number)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_pinned_issues(
        &self,
        req: Request<ListPinnedIssuesRequest>,
    ) -> Result<Response<ListPinnedIssuesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.list_pinned")
                .await?;
            let mut issues = self.forge.work.list_pinned(&cx, &Self::info(&repo)).await?;
            self.fix_names(&cx, &mut issues).await;
            self.inspire(&caller, &cx, &mut issues).await?;
            Ok(ListPinnedIssuesResponse { issues })
        }
        .await;
        reply(&caller, out)
    }

    async fn lock_issue(&self, req: Request<LockIssueRequest>) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.lock")
                .await?;
            let reason =
                IssueLockReason::try_from(r.reason).unwrap_or(IssueLockReason::Unspecified);
            let mut issue = self
                .forge
                .work
                .lock_issue(&cx, &Self::info(&repo), &actor, r.number, reason)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    async fn unlock_issue(
        &self,
        req: Request<UnlockIssueRequest>,
    ) -> Result<Response<Issue>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.unlock")
                .await?;
            let mut issue = self
                .forge
                .work
                .unlock_issue(&cx, &Self::info(&repo), &actor, r.number)
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(issue)
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- transfer ----------------

    async fn transfer_issue(
        &self,
        req: Request<TransferIssueRequest>,
    ) -> Result<Response<TransferIssueResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let src = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &src, Level::Write, "issue.transfer")
                .await?;
            let dst = self.repo(&caller, &r.target_repo).await?;
            // Write access to both repos, with the target audited like the source.
            let (_, _) = self
                .issue_access(&caller, &dst, Level::Write, "issue.transfer")
                .await?;
            let (mut issue, old_number) = self
                .forge
                .work
                .transfer_issue(
                    &cx,
                    &Self::info(&src),
                    &Self::info(&dst),
                    &actor,
                    r.number,
                    r.create_labels,
                )
                .await?;
            self.fix_one(&cx, &mut issue).await;
            self.inspire(&caller, &cx, std::slice::from_mut(&mut issue))
                .await?;
            Ok(TransferIssueResponse {
                issue: Some(issue),
                redirect_from: Some(IssueRef {
                    repo: Some(RepoRef {
                        repo: Some(repo_ref::Repo::FullName(src.full_name.clone())),
                    }),
                    number: old_number,
                    id: String::new(),
                }),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- timeline ----------------

    async fn list_issue_timeline(
        &self,
        req: Request<ListIssueTimelineRequest>,
    ) -> Result<Response<ListIssueTimelineResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.timeline")
                .await?;
            let (token, size) = page_size(&r.page);
            let mut res = self
                .forge
                .work
                .list_timeline(&cx, &Self::info(&repo), r.number, &r.kinds, size, &token)
                .await?;
            self.fix_names(&cx, &mut res.events).await;
            Ok(ListIssueTimelineResponse {
                events: res.events,
                page: next(res.next),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- development panel ----------------

    async fn link_issue_change(
        &self,
        req: Request<LinkIssueChangeRequest>,
    ) -> Result<Response<IssueLinkedChange>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.link_change")
                .await?;
            if r.change_request_id.is_empty() {
                return Err(ForgeError::missing("change_request_id"));
            }
            let (cr_repo_id, number) = self
                .forge
                .work
                .locate_change(&cx, &r.change_request_id)
                .await?
                .ok_or_else(|| {
                    ForgeError::not_found(format!("change request {}", r.change_request_id))
                })?;
            let cr_repo = self.issue_repo_readable(&caller, &cr_repo_id).await?;
            let info = self
                .forge
                .work
                .hooks()
                .changes
                .change(&cr_repo, &r.change_request_id)
                .await?;
            let change = ChangeLink {
                id: r.change_request_id.clone(),
                repo_id: cr_repo_id,
                number,
                title: info.as_ref().map(|c| c.title.clone()).unwrap_or_default(),
                state: info
                    .as_ref()
                    .map(|c| c.state.clone())
                    .unwrap_or_else(|| "open".into()),
            };
            let mut link = self
                .forge
                .work
                .link_change(
                    &cx,
                    &Self::info(&repo),
                    &actor.principal,
                    r.number,
                    change,
                    r.closes,
                    IssueLinkSource::Manual,
                )
                .await?;
            self.fix_one(&cx, &mut link).await;
            Ok(link)
        }
        .await;
        reply(&caller, out)
    }

    async fn unlink_issue_change(
        &self,
        req: Request<UnlinkIssueChangeRequest>,
    ) -> Result<Response<UnlinkIssueChangeResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Triage, "issue.unlink_change")
                .await?;
            self.forge
                .work
                .unlink_change(
                    &cx,
                    &Self::info(&repo),
                    &actor.principal,
                    r.number,
                    &r.change_request_id,
                )
                .await?;
            Ok(UnlinkIssueChangeResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_issue_development(
        &self,
        req: Request<GetIssueDevelopmentRequest>,
    ) -> Result<Response<IssueDevelopment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.development")
                .await?;
            let mut dev = self
                .forge
                .work
                .get_development(&cx, &Self::info(&repo), r.number)
                .await?;
            self.fix_names(&cx, &mut dev.changes).await;
            Ok(dev)
        }
        .await;
        reply(&caller, out)
    }

    async fn create_issue_branch(
        &self,
        req: Request<CreateIssueBranchRequest>,
    ) -> Result<Response<IssueLinkedBranch>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Write, "issue.create_branch")
                .await?;
            let target = match &r.target_repo {
                Some(t) if t.repo.is_some() => {
                    let t = self.repo(&caller, &r.target_repo).await?;
                    self.issue_access(&caller, &t, Level::Write, "issue.create_branch")
                        .await?;
                    t
                }
                _ => repo.clone(),
            };
            let issue = self
                .forge
                .work
                .get_issue_by_number(&cx, &Self::info(&repo), r.number)
                .await?;
            let ref_name = if r.ref_name.is_empty() {
                default_branch_name(issue.number, &issue.title)
            } else {
                r.ref_name.clone()
            };
            let base = if r.base_ref.is_empty() {
                repo.default_ref.clone()
            } else {
                r.base_ref.clone()
            };
            // This branch is provisioned directly by the VCS port.
            let fence =
                crate::safety::control::fence_nonhuman(&self.forge.db, &cx, &target.org_id).await?;
            let workspace = self
                .forge
                .work
                .hooks()
                .branches
                .create_branch(&target, &ref_name, &base, r.create_workspace)
                .await?;
            drop(fence);
            self.forge
                .work
                .record_branch(
                    &cx,
                    &Self::info(&repo),
                    &actor,
                    r.number,
                    &ref_name,
                    &base,
                    &workspace,
                    &target.full_name,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- task lists ----------------

    async fn get_issue_task_list_progress(
        &self,
        req: Request<GetIssueTaskListProgressRequest>,
    ) -> Result<Response<IssueTaskListProgress>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, _) = self
                .issue_access(&caller, &repo, Level::View, "issue.task_list")
                .await?;
            let issue = self
                .forge
                .work
                .get_issue_by_number(&cx, &Self::info(&repo), r.number)
                .await?;
            Ok(issue.task_list.unwrap_or_default())
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- saved replies ----------------

    async fn create_saved_reply(
        &self,
        req: Request<CreateSavedReplyRequest>,
    ) -> Result<Response<SavedReply>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.personal(&caller)?;
            self.forge
                .work
                .create_saved_reply(&cx, &r.title, &r.body)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_saved_reply(
        &self,
        req: Request<GetSavedReplyRequest>,
    ) -> Result<Response<SavedReply>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.personal(&caller)?;
            self.forge.work.get_saved_reply(&cx, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_saved_reply(
        &self,
        req: Request<UpdateSavedReplyRequest>,
    ) -> Result<Response<SavedReply>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.personal(&caller)?;
            self.forge
                .work
                .update_saved_reply(&cx, &r.id, r.title, r.body, r.expected_version)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_saved_reply(
        &self,
        req: Request<DeleteSavedReplyRequest>,
    ) -> Result<Response<DeleteSavedReplyResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.personal(&caller)?;
            self.forge.work.delete_saved_reply(&cx, &r.id).await?;
            Ok(DeleteSavedReplyResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_saved_replies(
        &self,
        req: Request<ListSavedRepliesRequest>,
    ) -> Result<Response<ListSavedRepliesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.personal(&caller)?;
            let (token, size) = page_size(&r.page);
            let (saved_replies, tok) = self
                .forge
                .work
                .list_saved_replies(&cx, &r.query, size, &token)
                .await?;
            Ok(ListSavedRepliesResponse {
                saved_replies,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }
}

impl ForgeServices {
    /// The context of a personal (per-user) call: the token must allow `read`.
    fn personal(&self, caller: &Caller) -> Result<crate::cx::Cx> {
        if !caller.token.allows_action(Action::Read) {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::PolicyDenied,
                "token does not allow `read`",
            ));
        }
        let mut cx = caller.cx.clone();
        cx.decision = "token:ok personal".into();
        Ok(cx)
    }

    /// Org members for an org request, or readers of the repo of a repo request.
    async fn org_or_repo_view(
        &self,
        caller: &Caller,
        org: &str,
        repo: &Option<RepoRef>,
    ) -> Result<(crate::cx::Cx, String)> {
        if !org.is_empty() {
            return self.org_view(caller, org).await;
        }
        if repo.as_ref().is_some_and(|r| r.repo.is_some()) {
            let repo = self.repo(caller, repo).await?;
            let (cx, _) = self
                .issue_access(caller, &repo, Level::View, "issue.view")
                .await?;
            return Ok((cx, repo.org_id));
        }
        Err(ForgeError::missing("org"))
    }
}
