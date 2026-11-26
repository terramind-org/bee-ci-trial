//! `ProjectService`: Projects v2. Access: org owners and admins administer an org's projects,
//! every org member writes, public projects are readable by any authenticated caller, and a
//! user's own projects belong to that user (`docs/spec/forge-work.md` §3.2).

use std::collections::BTreeMap;

use bee_policy::Action;
use bee_proto::v1::project_service_server::ProjectService;
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::work::Level;
use super::{ForgeServices, next, page, reply};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::work::{
    ChangeLink, CopyRequest, EvalCtx, IssueTarget, ItemContent, ItemQuery, OwnerInfo,
    ProjectListQuery, RepoInfo, ViewPatch, ViewSpec, WorkflowPatch, WorkflowSpec,
};

/// What a project call needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PLevel {
    /// Reads.
    View,
    /// Items, fields, views, charts, status updates.
    Write,
    /// Settings, workflows, links, deletion.
    Admin,
}

impl ForgeServices {
    async fn owner_of(&self, caller: &Caller, owner: &str) -> Result<OwnerInfo> {
        if owner.is_empty() {
            return Err(ForgeError::missing("owner"));
        }
        match self.forge.orgs.get_org(&caller.cx, owner).await {
            Ok(o) => Ok(OwnerInfo {
                id: o.id,
                login: o.name,
                kind: ProjectOwnerKind::Org,
            }),
            Err(e) if e.reason == ErrorReason::NotFound => {
                let p = self
                    .principal_of(owner)
                    .await
                    .map_err(|_| ForgeError::not_found(format!("owner {owner}")))?;
                let login = self.login_of(&p.id).await;
                Ok(OwnerInfo {
                    id: p.id,
                    login,
                    kind: ProjectOwnerKind::User,
                })
            }
            Err(e) => Err(e),
        }
    }

    fn org_token_ok(caller: &Caller, write: bool) -> Result<()> {
        if write {
            if caller.token.scope.repos.is_some() {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    "project changes need a token not restricted to repos",
                ));
            }
            if !caller.token.allows_action(Action::Triage) {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    "token does not allow `triage`",
                ));
            }
        } else if !caller.token.allows_action(Action::Read) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `read`",
            ));
        }
        Ok(())
    }

    /// Authorizes a call on an owner (org or user) at `level`; `public` marks a readable
    /// public project. Denied state changes are audited as `audit`.
    #[allow(clippy::too_many_arguments)]
    async fn owner_access(
        &self,
        caller: &Caller,
        owner_id: &str,
        kind: ProjectOwnerKind,
        creator: &str,
        public: bool,
        level: PLevel,
        audit: &str,
    ) -> Result<Cx> {
        let write = level != PLevel::View;
        let org = if kind == ProjectOwnerKind::Org {
            owner_id.to_string()
        } else {
            String::new()
        };
        let result: Result<Cx> = async {
            if kind == ProjectOwnerKind::User {
                let own = caller.cx.chain.iter().any(|c| c == owner_id);
                Self::org_token_ok(caller, write)?;
                if own {
                    let mut cx = caller.cx.clone();
                    cx.decision = "token:ok owner".into();
                    return Ok(cx);
                }
                if !write && public {
                    let mut cx = caller.cx.clone();
                    cx.decision = "token:ok public project".into();
                    return Ok(cx);
                }
                return Err(if write && public {
                    ForgeError::permission_denied("only the owner may change a user's project")
                } else {
                    ForgeError::not_found("project")
                });
            }
            let member = self
                .forge
                .authz
                .org(caller, owner_id, OrgPerm::ViewMembers)
                .await;
            match member {
                Ok(mut cx) => {
                    Self::org_token_ok(caller, write)?;
                    if level == PLevel::Admin {
                        let admin = self
                            .forge
                            .authz
                            .org(caller, owner_id, OrgPerm::Manage)
                            .await;
                        let creator_ok =
                            !creator.is_empty() && caller.cx.chain.iter().any(|c| c == creator);
                        match admin {
                            Ok(c) => cx = c,
                            Err(_) if creator_ok => {}
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(cx)
                }
                Err(_) if !write && public => {
                    Self::org_token_ok(caller, false)?;
                    let mut cx = caller.cx.clone();
                    cx.decision = "token:ok public project".into();
                    Ok(cx)
                }
                Err(_) if write && public => Err(ForgeError::permission_denied(
                    "only org members may change this project",
                )),
                Err(_) => Err(ForgeError::not_found("project")),
            }
        }
        .await;
        if write {
            self.guarded(caller, &org, "", audit, result).await
        } else {
            result
        }
    }

    /// Loads a project and authorizes `level`.
    pub(super) async fn project_for(
        &self,
        caller: &Caller,
        project_id: &str,
        level: PLevel,
        audit: &str,
    ) -> Result<(Cx, Project)> {
        if project_id.is_empty() {
            return Err(ForgeError::missing("project_id"));
        }
        let p = self.forge.work.get_project(&caller.cx, project_id).await?;
        let creator = p.creator.as_ref().map(|c| c.id.clone()).unwrap_or_default();
        let kind = ProjectOwnerKind::try_from(p.owner_kind).unwrap_or(ProjectOwnerKind::Org);
        let cx = self
            .owner_access(caller, &p.owner_id, kind, &creator, p.public, level, audit)
            .await?;
        if level == PLevel::Write && p.closed {
            return Err(ForgeError::invalid_state("the project is closed"));
        }
        Ok((cx, p))
    }

    /// Write access to a project, for callers outside the project service.
    pub(super) async fn project_write(
        &self,
        caller: &Caller,
        project_id: &str,
        audit: &str,
    ) -> Result<()> {
        self.project_for(caller, project_id, PLevel::Write, audit)
            .await
            .map(|_| ())
    }

    async fn project_actor(&self, cx: &Cx) -> crate::work::Actor {
        let principal = self
            .forge
            .hooks
            .directory
            .get(&cx.actor_id)
            .await
            .unwrap_or_else(|| cx.principal());
        crate::work::Actor::staff(principal)
    }

    /// Whether the caller may read the repo `id` (a deleted or missing repo reads as no),
    /// remembered in `seen`.
    async fn repo_readable(
        &self,
        caller: &Caller,
        id: &str,
        seen: &mut BTreeMap<String, bool>,
    ) -> bool {
        if let Some(b) = seen.get(id) {
            return *b;
        }
        let ok = match self
            .forge
            .orgs
            .resolve_repo(
                &caller.cx,
                &RepoRef {
                    repo: Some(repo_ref::Repo::Id(id.to_string())),
                },
            )
            .await
        {
            Ok(r) => self
                .forge
                .authz
                .repo(caller, &r, Action::Read, &[], None, None)
                .await
                .is_ok(),
            Err(_) => false,
        };
        seen.insert(id.to_string(), ok);
        ok
    }

    /// Items whose content the caller cannot read become `REDACTED` placeholders.
    async fn redact(&self, caller: &Caller, items: Vec<ProjectItem>) -> Vec<ProjectItem> {
        let mut seen: BTreeMap<String, bool> = BTreeMap::new();
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let repo_id = it
                .content
                .as_ref()
                .and_then(|c| c.repo.as_ref())
                .and_then(|r| match &r.repo {
                    Some(repo_ref::Repo::Id(i)) => Some(i.clone()),
                    _ => None,
                });
            let readable = match &repo_id {
                None => true,
                Some(id) => self.repo_readable(caller, id, &mut seen).await,
            };
            if readable {
                out.push(it);
            } else {
                out.push(ProjectItem {
                    id: it.id,
                    project_id: it.project_id,
                    kind: ProjectItemKind::Redacted as i32,
                    content_state: String::new(),
                    archived: it.archived,
                    position: it.position,
                    created_at: it.created_at,
                    updated_at: it.updated_at,
                    archived_at: it.archived_at,
                    version: it.version,
                    ..Default::default()
                });
            }
        }
        out
    }

    async fn item_out(&self, caller: &Caller, cx: &Cx, item: ProjectItem) -> ProjectItem {
        let mut v = self.redact(caller, vec![item]).await;
        self.fix_names(cx, &mut v).await;
        v.pop().unwrap_or_default()
    }

    /// Logins and repo names named in filters, resolved for evaluation.
    async fn eval_ctx(&self, caller: &Caller, filters: &[&str]) -> Result<EvalCtx> {
        let mut ctx = EvalCtx {
            me: caller.cx.actor_id.clone(),
            now_ms: self.forge.db.now_ms(),
            ..Default::default()
        };
        for f in filters {
            let (people, repos) = crate::work::pfilter::references(f)?;
            for l in people {
                if let Ok(p) = self.principal_of(&l).await {
                    ctx.people.insert(l.to_lowercase(), p.id);
                }
            }
            for r in repos {
                let rr = RepoRef {
                    repo: Some(repo_ref::Repo::FullName(r.clone())),
                };
                if let Ok(repo) = self.forge.orgs.resolve_repo(&caller.cx, &rr).await {
                    ctx.repos.insert(r.to_lowercase(), repo.id);
                }
            }
        }
        Ok(ctx)
    }

    /// Resolves an item content reference for `AddProjectItem`.
    async fn item_content(&self, caller: &Caller, c: &SubjectRef) -> Result<ItemContent> {
        match SubjectKind::try_from(c.kind).unwrap_or(SubjectKind::Unspecified) {
            SubjectKind::Issue => {
                let numeric =
                    !c.id.is_empty() && c.id.len() < 12 && c.id.bytes().all(|b| b.is_ascii_digit());
                let (target, repo) = if numeric {
                    let repo = self.repo(caller, &c.repo).await?;
                    (
                        IssueTarget::Number {
                            repo_id: repo.id.clone(),
                            number: c.id.parse().unwrap_or(0),
                        },
                        repo,
                    )
                } else if !c.id.is_empty() {
                    let repo_id = self.forge.work.issue_repo(&caller.cx, &c.id).await?;
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
                    (IssueTarget::Id(c.id.clone()), repo)
                } else {
                    return Err(ForgeError::missing("content.id"));
                };
                self.forge
                    .authz
                    .repo(caller, &repo, Action::Read, &[], None, None)
                    .await?;
                Ok(ItemContent::Issue(target))
            }
            SubjectKind::ChangeRequest => {
                let repo = self.repo(caller, &c.repo).await?;
                self.forge
                    .authz
                    .repo(caller, &repo, Action::Read, &[], None, None)
                    .await?;
                let work = &self.forge.work;
                let id = if !c.id.is_empty()
                    && c.id.bytes().all(|b| b.is_ascii_digit())
                    && c.id.len() < 12
                {
                    let n: u64 = c.id.parse().unwrap_or(0);
                    match work.number_entry(&caller.cx, &repo.id, n).await? {
                        Some(e) if e.kind() == crate::work::NumberKind::ChangeRequest => {
                            e.subject_id
                        }
                        _ => return Err(ForgeError::not_found(format!("change request #{n}"))),
                    }
                } else {
                    c.id.clone()
                };
                let (repo_id, number) = work
                    .locate_change(&caller.cx, &id)
                    .await?
                    .ok_or_else(|| ForgeError::not_found(format!("change request {id}")))?;
                if repo_id != repo.id {
                    return Err(ForgeError::not_found(format!("change request {id}")));
                }
                let info = work.hooks().changes.change(&repo, &id).await?;
                Ok(ItemContent::Change {
                    change: ChangeLink {
                        id,
                        repo_id,
                        number,
                        title: info.as_ref().map(|i| i.title.clone()).unwrap_or_default(),
                        state: info
                            .as_ref()
                            .map(|i| i.state.clone())
                            .unwrap_or_else(|| "open".into()),
                    },
                    author_id: String::new(),
                })
            }
            _ => Err(ForgeError::invalid(
                "invalid_content",
                "project items are issues or change requests; use CreateProjectDraftItem for drafts",
            )),
        }
    }

    /// Resolves a repo for linking or automation: readable by the caller.
    async fn linked_repo(&self, caller: &Caller, r: &Option<RepoRef>) -> Result<RepoInfo> {
        let repo = self.repo(caller, r).await?;
        self.forge
            .authz
            .repo(caller, &repo, Action::Read, &[], None, None)
            .await?;
        Ok(RepoInfo::from(&repo))
    }
}

fn opt<T>(v: T, set: bool) -> Option<T> {
    set.then_some(v)
}

#[tonic::async_trait]
impl ProjectService for ForgeServices {
    // ---------------- projects ----------------

    async fn create_project(
        &self,
        req: Request<CreateProjectRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let owner = self.owner_of(&caller, &r.owner).await?;
            let cx = self
                .owner_access(
                    &caller,
                    &owner.id,
                    owner.kind,
                    "",
                    false,
                    PLevel::Write,
                    "project.create",
                )
                .await?;
            let actor = self.project_actor(&cx).await;
            let mut repos = Vec::new();
            for l in &r.link_repos {
                repos.push(self.linked_repo(&caller, &Some(l.clone())).await?);
            }
            self.forge
                .work
                .create_project(
                    &cx,
                    &owner,
                    &actor,
                    &r.title,
                    &r.short_description,
                    &r.readme,
                    r.public,
                    &repos,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_project(
        &self,
        req: Request<GetProjectRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            Ok(self
                .project_for(&caller, &r.project_id, PLevel::View, "project.get")
                .await?
                .1)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_project_by_number(
        &self,
        req: Request<GetProjectByNumberRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let owner = self.owner_of(&caller, &r.owner).await?;
            let p = self
                .forge
                .work
                .get_project_by_number(&caller.cx, &owner.id, r.number)
                .await?;
            self.project_for(&caller, &p.id, PLevel::View, "project.get")
                .await
                .map(|x| x.1)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_projects(
        &self,
        req: Request<ListProjectsRequest>,
    ) -> Result<Response<ListProjectsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner_id, repo_id, org_for_member) =
                if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                    let repo = self.repo(&caller, &r.repo).await?;
                    self.forge
                        .authz
                        .repo(&caller, &repo, Action::Read, &[], None, None)
                        .await?;
                    (String::new(), repo.id.clone(), Some(repo.org_id))
                } else {
                    let owner = self.owner_of(&caller, &r.owner).await?;
                    let org = (owner.kind == ProjectOwnerKind::Org).then(|| owner.id.clone());
                    (owner.id, String::new(), org)
                };
            let member = match &org_for_member {
                Some(org) => self
                    .forge
                    .authz
                    .org(&caller, org, OrgPerm::ViewMembers)
                    .await
                    .is_ok(),
                None => caller.cx.chain.contains(&owner_id),
            };
            let (token, size) = page(&r.page);
            let (projects, tok) = self
                .forge
                .work
                .list_projects(
                    &caller.cx,
                    &ProjectListQuery {
                        owner_id,
                        repo_id,
                        query: r.query.clone(),
                        include_closed: r.include_closed,
                        templates_only: r.templates_only,
                        public_only: !member,
                    },
                    size,
                    &token,
                )
                .await?;
            Ok(ListProjectsResponse {
                projects,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_project(
        &self,
        req: Request<UpdateProjectRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Admin, "project.update")
                .await?;
            self.forge
                .work
                .update_project(
                    &cx,
                    &r.project_id,
                    r.title,
                    r.short_description,
                    r.readme,
                    r.public,
                    r.closed,
                    r.is_template,
                    r.expected_version,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_project(
        &self,
        req: Request<DeleteProjectRequest>,
    ) -> Result<Response<DeleteProjectResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Admin, "project.delete")
                .await?;
            let actor = self.project_actor(&cx).await;
            self.forge
                .work
                .delete_project(&cx, &actor, &r.project_id)
                .await?;
            Ok(DeleteProjectResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn copy_project(
        &self,
        req: Request<CopyProjectRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            // Anyone who can read the source may copy it; templates are meant for that.
            let (_, src) = self
                .project_for(&caller, &r.source_project_id, PLevel::View, "project.copy")
                .await?;
            let owner = if r.owner.is_empty() {
                OwnerInfo {
                    id: src.owner_id.clone(),
                    login: src.owner_login.clone(),
                    kind: ProjectOwnerKind::try_from(src.owner_kind)
                        .unwrap_or(ProjectOwnerKind::Org),
                }
            } else {
                self.owner_of(&caller, &r.owner).await?
            };
            let cx = self
                .owner_access(
                    &caller,
                    &owner.id,
                    owner.kind,
                    "",
                    false,
                    PLevel::Write,
                    "project.copy",
                )
                .await?;
            let actor = self.project_actor(&cx).await;
            let title = if r.title.is_empty() {
                format!("{} (copy)", src.title)
            } else {
                r.title.clone()
            };
            self.forge
                .work
                .copy_project(
                    &cx,
                    &actor,
                    &r.source_project_id,
                    CopyRequest {
                        owner,
                        title,
                        include_drafts: r.include_draft_items,
                        public: r.public,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- items ----------------

    async fn add_project_item(
        &self,
        req: Request<AddProjectItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Write, "project_item.add")
                .await?;
            let actor = self.project_actor(&cx).await;
            let content = r
                .content
                .as_ref()
                .ok_or_else(|| ForgeError::missing("content"))?;
            let content = self.item_content(&caller, content).await?;
            let item = self
                .forge
                .work
                .add_item(&cx, &actor, &r.project_id, content)
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn create_project_draft_item(
        &self,
        req: Request<CreateProjectDraftItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_item.create_draft",
                )
                .await?;
            let actor = self.project_actor(&cx).await;
            let mut who = Vec::new();
            for h in &r.assignees {
                who.push(self.principal_of(h).await?);
            }
            let item = self
                .forge
                .work
                .create_draft_item(&cx, &actor, &r.project_id, &r.title, &r.body, &who)
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn update_project_draft_item(
        &self,
        req: Request<UpdateProjectDraftItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_item.update_draft",
                )
                .await?;
            let who = if r.set_assignees {
                let mut v = Vec::new();
                for h in &r.assignees {
                    v.push(self.principal_of(h).await?);
                }
                Some(v)
            } else {
                None
            };
            let item = self
                .forge
                .work
                .update_draft_item(
                    &cx,
                    &r.project_id,
                    &r.item_id,
                    r.title,
                    r.body,
                    who.as_deref(),
                    r.expected_version,
                )
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn convert_project_draft_item(
        &self,
        req: Request<ConvertProjectDraftItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_item.convert_draft",
                )
                .await?;
            let repo = self.repo(&caller, &r.repo).await?;
            let (cx, actor) = self
                .issue_access(&caller, &repo, Level::Read, "issue.create")
                .await?;
            let item = self
                .forge
                .work
                .convert_draft_item(
                    &cx,
                    &actor,
                    &RepoInfo::from(&repo),
                    &r.project_id,
                    &r.item_id,
                )
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn get_project_item(
        &self,
        req: Request<GetProjectItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::View, "project_item.get")
                .await?;
            let item = self
                .forge
                .work
                .get_item(&cx, &r.project_id, &r.item_id)
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_project_items(
        &self,
        req: Request<ListProjectItemsRequest>,
    ) -> Result<Response<ListProjectItemsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::View, "project_item.list")
                .await?;
            let view_filter = if r.view_id.is_empty() {
                String::new()
            } else {
                self.forge
                    .work
                    .get_view(&cx, &r.project_id, &r.view_id)
                    .await?
                    .filter
            };
            let ctx = self.eval_ctx(&caller, &[&r.query, &view_filter]).await?;
            // Filtering or sorting must not look at items the caller cannot read (see
            // `ItemQuery::hidden_repos`); a plain listing only redacts them.
            let mut hidden_repos = std::collections::BTreeSet::new();
            if !r.query.trim().is_empty() || !r.view_id.is_empty() {
                let mut seen = BTreeMap::new();
                for id in self.forge.work.item_repo_ids(&cx, &r.project_id).await? {
                    if !self.repo_readable(&caller, &id, &mut seen).await {
                        hidden_repos.insert(id);
                    }
                }
            }
            let (token, size) = page(&r.page);
            let (items, total, tok) = self
                .forge
                .work
                .list_items(
                    &cx,
                    &r.project_id,
                    &ItemQuery {
                        query: r.query.clone(),
                        view_id: r.view_id.clone(),
                        include_archived: r.include_archived,
                        archived_only: r.archived_only,
                        field_ids: r.field_ids.clone(),
                        ctx,
                        hidden_repos,
                    },
                    size,
                    &token,
                )
                .await?;
            let mut items = self.redact(&caller, items).await;
            self.fix_names(&cx, &mut items).await;
            Ok(ListProjectItemsResponse {
                items,
                total_count: total,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn move_project_item(
        &self,
        req: Request<MoveProjectItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Write, "project_item.move")
                .await?;
            let item = self
                .forge
                .work
                .move_item(&cx, &r.project_id, &r.item_id, &r.after_item_id)
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn archive_project_item(
        &self,
        req: Request<ArchiveProjectItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_item.archive",
                )
                .await?;
            let item = self
                .forge
                .work
                .archive_item(&cx, &r.project_id, &r.item_id)
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn unarchive_project_item(
        &self,
        req: Request<UnarchiveProjectItemRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_item.unarchive",
                )
                .await?;
            let item = self
                .forge
                .work
                .unarchive_item(&cx, &r.project_id, &r.item_id)
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_project_item(
        &self,
        req: Request<DeleteProjectItemRequest>,
    ) -> Result<Response<DeleteProjectItemResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Write, "project_item.delete")
                .await?;
            let actor = self.project_actor(&cx).await;
            self.forge
                .work
                .delete_item(&cx, &actor, &r.project_id, &r.item_id)
                .await?;
            Ok(DeleteProjectItemResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- fields ----------------

    async fn create_project_field(
        &self,
        req: Request<CreateProjectFieldRequest>,
    ) -> Result<Response<ProjectField>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.create_field",
                )
                .await?;
            self.forge
                .work
                .create_project_field(
                    &cx,
                    &r.project_id,
                    &r.name,
                    ProjectFieldKind::try_from(r.kind).unwrap_or(ProjectFieldKind::Unspecified),
                    &r.options,
                    r.iteration_config.as_ref(),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_project_field(
        &self,
        req: Request<UpdateProjectFieldRequest>,
    ) -> Result<Response<ProjectField>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.update_field",
                )
                .await?;
            self.forge
                .work
                .update_project_field(
                    &cx,
                    &r.project_id,
                    &r.field_id,
                    r.name,
                    opt(r.options.as_slice(), r.set_options),
                    r.iteration_config.as_ref(),
                    r.expected_version,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_project_field(
        &self,
        req: Request<DeleteProjectFieldRequest>,
    ) -> Result<Response<DeleteProjectFieldResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.delete_field",
                )
                .await?;
            self.forge
                .work
                .delete_project_field(&cx, &r.project_id, &r.field_id)
                .await?;
            Ok(DeleteProjectFieldResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_project_fields(
        &self,
        req: Request<ListProjectFieldsRequest>,
    ) -> Result<Response<ListProjectFieldsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_config.list_fields",
                )
                .await?;
            let (token, size) = page(&r.page);
            let (fields, tok) = self
                .forge
                .work
                .list_project_fields(&cx, &r.project_id, size, &token)
                .await?;
            Ok(ListProjectFieldsResponse {
                fields,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_project_item_field_value(
        &self,
        req: Request<SetProjectItemFieldValueRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_item.set_value",
                )
                .await?;
            let actor = self.project_actor(&cx).await;
            let value = r
                .value
                .as_ref()
                .ok_or_else(|| ForgeError::missing("value"))?;
            let item = self
                .forge
                .work
                .set_item_value(
                    &cx,
                    &actor,
                    &r.project_id,
                    &r.item_id,
                    value,
                    r.expected_version,
                )
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    async fn clear_project_item_field_value(
        &self,
        req: Request<ClearProjectItemFieldValueRequest>,
    ) -> Result<Response<ProjectItem>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_item.clear_value",
                )
                .await?;
            let actor = self.project_actor(&cx).await;
            let item = self
                .forge
                .work
                .clear_item_value(&cx, &actor, &r.project_id, &r.item_id, &r.field_id)
                .await?;
            Ok(self.item_out(&caller, &cx, item).await)
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- views ----------------

    async fn create_project_view(
        &self,
        req: Request<CreateProjectViewRequest>,
    ) -> Result<Response<ProjectView>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.create_view",
                )
                .await?;
            self.forge
                .work
                .create_view(
                    &cx,
                    &r.project_id,
                    ViewSpec {
                        name: r.name,
                        layout: ProjectViewLayout::try_from(r.layout)
                            .unwrap_or(ProjectViewLayout::Unspecified),
                        filter: r.filter,
                        group_by_field_ids: r.group_by_field_ids,
                        column_field_id: r.column_field_id,
                        slice_by_field_id: r.slice_by_field_id,
                        sort_by: r.sort_by,
                        visible_field_ids: r.visible_field_ids,
                        sum_field_ids: r.sum_field_ids,
                        start_date_field_id: r.start_date_field_id,
                        target_date_field_id: r.target_date_field_id,
                        roadmap_zoom: ProjectRoadmapZoom::try_from(r.roadmap_zoom)
                            .unwrap_or(ProjectRoadmapZoom::Unspecified),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_project_view(
        &self,
        req: Request<GetProjectViewRequest>,
    ) -> Result<Response<ProjectView>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_config.get_view",
                )
                .await?;
            self.forge
                .work
                .get_view(&cx, &r.project_id, &r.view_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_project_view(
        &self,
        req: Request<UpdateProjectViewRequest>,
    ) -> Result<Response<ProjectView>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.update_view",
                )
                .await?;
            let patch = ViewPatch {
                name: r.name,
                layout: ProjectViewLayout::try_from(r.layout)
                    .unwrap_or(ProjectViewLayout::Unspecified),
                filter: r.filter,
                group_by: r.set_group_by.then_some(r.group_by_field_ids),
                column_field_id: r.column_field_id,
                slice_by_field_id: r.slice_by_field_id,
                sort_by: r.set_sort_by.then_some(r.sort_by),
                visible_field_ids: r.set_visible_fields.then_some(r.visible_field_ids),
                sum_field_ids: r.set_sum_fields.then_some(r.sum_field_ids),
                start_date_field_id: r.start_date_field_id,
                target_date_field_id: r.target_date_field_id,
                roadmap_zoom: ProjectRoadmapZoom::try_from(r.roadmap_zoom)
                    .unwrap_or(ProjectRoadmapZoom::Unspecified),
                position: r.position,
            };
            self.forge
                .work
                .update_view(&cx, &r.project_id, &r.view_id, patch, r.expected_version)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_project_view(
        &self,
        req: Request<DeleteProjectViewRequest>,
    ) -> Result<Response<DeleteProjectViewResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.delete_view",
                )
                .await?;
            self.forge
                .work
                .delete_view(&cx, &r.project_id, &r.view_id)
                .await?;
            Ok(DeleteProjectViewResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_project_views(
        &self,
        req: Request<ListProjectViewsRequest>,
    ) -> Result<Response<ListProjectViewsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_config.list_views",
                )
                .await?;
            let (token, size) = page(&r.page);
            let (views, tok) = self
                .forge
                .work
                .list_views(&cx, &r.project_id, size, &token)
                .await?;
            Ok(ListProjectViewsResponse {
                views,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- iterations ----------------

    async fn list_project_iterations(
        &self,
        req: Request<ListProjectIterationsRequest>,
    ) -> Result<Response<ListProjectIterationsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_config.list_iterations",
                )
                .await?;
            let iterations = self
                .forge
                .work
                .list_iterations(&cx, &r.project_id, &r.field_id, r.include_completed)
                .await?;
            Ok(ListProjectIterationsResponse { iterations })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_project_iteration(
        &self,
        req: Request<AddProjectIterationRequest>,
    ) -> Result<Response<ProjectIteration>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.add_iteration",
                )
                .await?;
            self.forge
                .work
                .add_iteration(
                    &cx,
                    &r.project_id,
                    &r.field_id,
                    &r.title,
                    r.start_on.as_ref().map(ts_ms),
                    r.duration_days,
                    r.is_break,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn complete_project_iteration(
        &self,
        req: Request<CompleteProjectIterationRequest>,
    ) -> Result<Response<ProjectIteration>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.complete_iteration",
                )
                .await?;
            self.forge
                .work
                .complete_iteration(
                    &cx,
                    &r.project_id,
                    &r.field_id,
                    &r.iteration_id,
                    &r.move_open_items_to_iteration_id,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- workflows ----------------

    async fn list_project_workflows(
        &self,
        req: Request<ListProjectWorkflowsRequest>,
    ) -> Result<Response<ListProjectWorkflowsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_config.list_workflows",
                )
                .await?;
            Ok(ListProjectWorkflowsResponse {
                workflows: self.forge.work.list_workflows(&cx, &r.project_id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_project_workflow(
        &self,
        req: Request<CreateProjectWorkflowRequest>,
    ) -> Result<Response<ProjectWorkflow>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Admin,
                    "project_config.create_workflow",
                )
                .await?;
            let source_repo = match &r.source_repo {
                Some(s) if s.repo.is_some() => {
                    Some(self.linked_repo(&caller, &r.source_repo).await?)
                }
                _ => None,
            };
            self.forge
                .work
                .create_workflow(
                    &cx,
                    &r.project_id,
                    WorkflowSpec {
                        kind: ProjectWorkflowKind::try_from(r.kind)
                            .unwrap_or(ProjectWorkflowKind::Unspecified),
                        name: r.name,
                        filter: r.filter,
                        source_repo,
                        field_id: r.field_id,
                        option_id: r.option_id,
                        archive_after_days: r.archive_after_days,
                        enabled: r.enabled,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_project_workflow(
        &self,
        req: Request<UpdateProjectWorkflowRequest>,
    ) -> Result<Response<ProjectWorkflow>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Admin,
                    "project_config.update_workflow",
                )
                .await?;
            let source_repo = match &r.source_repo {
                Some(s) if s.repo.is_some() => {
                    Some(self.linked_repo(&caller, &r.source_repo).await?)
                }
                _ => None,
            };
            self.forge
                .work
                .update_workflow(
                    &cx,
                    &r.project_id,
                    &r.workflow_id,
                    WorkflowPatch {
                        name: r.name,
                        filter: r.filter,
                        source_repo,
                        field_id: r.field_id,
                        option_id: r.option_id,
                        archive_after_days: r.archive_after_days,
                    },
                    r.expected_version,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn enable_project_workflow(
        &self,
        req: Request<EnableProjectWorkflowRequest>,
    ) -> Result<Response<ProjectWorkflow>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Admin,
                    "project_config.enable_workflow",
                )
                .await?;
            self.forge
                .work
                .enable_workflow(&cx, &r.project_id, &r.workflow_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn disable_project_workflow(
        &self,
        req: Request<DisableProjectWorkflowRequest>,
    ) -> Result<Response<ProjectWorkflow>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Admin,
                    "project_config.disable_workflow",
                )
                .await?;
            self.forge
                .work
                .disable_workflow(&cx, &r.project_id, &r.workflow_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_project_workflow(
        &self,
        req: Request<DeleteProjectWorkflowRequest>,
    ) -> Result<Response<DeleteProjectWorkflowResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Admin,
                    "project_config.delete_workflow",
                )
                .await?;
            self.forge
                .work
                .delete_workflow(&cx, &r.project_id, &r.workflow_id)
                .await?;
            Ok(DeleteProjectWorkflowResponse {})
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- charts ----------------

    async fn create_project_chart(
        &self,
        req: Request<CreateProjectChartRequest>,
    ) -> Result<Response<ProjectChart>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.create_chart",
                )
                .await?;
            let chart = r.chart.ok_or_else(|| ForgeError::missing("chart"))?;
            self.forge
                .work
                .create_chart(&cx, &r.project_id, chart)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_project_chart(
        &self,
        req: Request<UpdateProjectChartRequest>,
    ) -> Result<Response<ProjectChart>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.update_chart",
                )
                .await?;
            let chart = r.chart.ok_or_else(|| ForgeError::missing("chart"))?;
            self.forge
                .work
                .update_chart(&cx, &r.project_id, &r.chart_id, chart, r.expected_version)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_project_chart(
        &self,
        req: Request<DeleteProjectChartRequest>,
    ) -> Result<Response<DeleteProjectChartResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_config.delete_chart",
                )
                .await?;
            self.forge
                .work
                .delete_chart(&cx, &r.project_id, &r.chart_id)
                .await?;
            Ok(DeleteProjectChartResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_project_charts(
        &self,
        req: Request<ListProjectChartsRequest>,
    ) -> Result<Response<ListProjectChartsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_config.list_charts",
                )
                .await?;
            Ok(ListProjectChartsResponse {
                charts: self.forge.work.list_charts(&cx, &r.project_id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn query_project_chart(
        &self,
        req: Request<QueryProjectChartRequest>,
    ) -> Result<Response<QueryProjectChartResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_config.query_chart",
                )
                .await?;
            let filter = match (&r.definition, r.chart_id.is_empty()) {
                (Some(d), true) => d.filter.clone(),
                _ => self
                    .forge
                    .work
                    .list_charts(&cx, &r.project_id)
                    .await?
                    .into_iter()
                    .find(|c| c.id == r.chart_id)
                    .map(|c| c.filter)
                    .unwrap_or_default(),
            };
            let ctx = self.eval_ctx(&caller, &[&filter]).await?;
            let (mut series, at) = self
                .forge
                .work
                .query_chart(&cx, &r.project_id, &r.chart_id, r.definition, &ctx)
                .await?;
            self.fix_names(&cx, &mut series).await;
            Ok(QueryProjectChartResponse {
                series,
                computed_at: Some(crate::ids::ts(at)),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- status updates ----------------

    async fn create_project_status_update(
        &self,
        req: Request<CreateProjectStatusUpdateRequest>,
    ) -> Result<Response<ProjectStatusUpdate>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_status_update.create",
                )
                .await?;
            let actor = self.project_actor(&cx).await;
            self.forge
                .work
                .create_status_update(
                    &cx,
                    &actor,
                    &r.project_id,
                    ProjectStatus::try_from(r.status).unwrap_or(ProjectStatus::Unspecified),
                    r.start_on.as_ref().map(ts_ms),
                    r.target_on.as_ref().map(ts_ms),
                    &r.body,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_project_status_update(
        &self,
        req: Request<UpdateProjectStatusUpdateRequest>,
    ) -> Result<Response<ProjectStatusUpdate>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_status_update.update",
                )
                .await?;
            self.forge
                .work
                .update_status_update(
                    &cx,
                    &r.project_id,
                    &r.status_update_id,
                    ProjectStatus::try_from(r.status).unwrap_or(ProjectStatus::Unspecified),
                    r.start_on.as_ref().map(ts_ms),
                    r.target_on.as_ref().map(ts_ms),
                    r.body,
                    r.expected_version,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_project_status_update(
        &self,
        req: Request<DeleteProjectStatusUpdateRequest>,
    ) -> Result<Response<DeleteProjectStatusUpdateResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::Write,
                    "project_status_update.delete",
                )
                .await?;
            self.forge
                .work
                .delete_status_update(&cx, &r.project_id, &r.status_update_id)
                .await?;
            Ok(DeleteProjectStatusUpdateResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_project_status_updates(
        &self,
        req: Request<ListProjectStatusUpdatesRequest>,
    ) -> Result<Response<ListProjectStatusUpdatesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(
                    &caller,
                    &r.project_id,
                    PLevel::View,
                    "project_status_update.list",
                )
                .await?;
            let (token, size) = page(&r.page);
            let (status_updates, tok) = self
                .forge
                .work
                .list_status_updates(&cx, &r.project_id, size, &token)
                .await?;
            Ok(ListProjectStatusUpdatesResponse {
                status_updates,
                page: next(tok),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---------------- links ----------------

    async fn link_project_repo(
        &self,
        req: Request<LinkProjectRepoRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Admin, "project.link_repo")
                .await?;
            let repo = self.linked_repo(&caller, &r.repo).await?;
            self.forge
                .work
                .link_project_repo(&cx, &r.project_id, &repo)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unlink_project_repo(
        &self,
        req: Request<UnlinkProjectRepoRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Admin, "project.unlink_repo")
                .await?;
            let repo = self.linked_repo(&caller, &r.repo).await?;
            self.forge
                .work
                .unlink_project_repo(&cx, &r.project_id, &repo)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn link_project_team(
        &self,
        req: Request<LinkProjectTeamRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Admin, "project.link_team")
                .await?;
            self.forge
                .work
                .link_project_team(
                    &cx,
                    &r.project_id,
                    &r.team,
                    ProjectTeamRole::try_from(r.role).unwrap_or(ProjectTeamRole::Unspecified),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn unlink_project_team(
        &self,
        req: Request<UnlinkProjectTeamRequest>,
    ) -> Result<Response<Project>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _) = self
                .project_for(&caller, &r.project_id, PLevel::Admin, "project.unlink_team")
                .await?;
            self.forge
                .work
                .unlink_project_team(&cx, &r.project_id, &r.team)
                .await
        }
        .await;
        reply(&caller, out)
    }
}
