//! The tonic `AgentService` and `WorkspaceService` over [`Agents`], following the forge service
//! shape (`docs/spec/forge-core.md` §7): authenticate, resolve, authorize (denials of
//! state-changing calls are audited), refuse writes to archived repos, call the store.
//!
//! | RPCs | needs |
//! |---|---|
//! | runtimes: register, update, delete | org owner or admin |
//! | runtimes: get, list; agent principals, dashboards, work items | org member (public runtimes: any caller) |
//! | start a run | `commit` on the repo (`review` for review tasks), and being the agent, one of its owners or a repo maintainer |
//! | cancel, retry, steer a run | `commit` on the repo, and being the starter, the agent, one of its owners or a repo maintainer |
//! | runs, events, instructions, watchers, anomalies, review config: read | `read` |
//! | repo memory and bundles: write | `commit` (bundles: owner or maintainer to change) |
//! | org memory: write | org owner or admin; agent memory: the agent or its owners |
//! | watchers, review config: write | `manage_settings` (maintainer) |
//! | report an anomaly | the watcher's agent, or `commit` |
//! | acknowledge, link an anomaly | `triage` |
//! | workspaces: create | `read`; for an agent-owned workspace, owning the agent |
//! | workspaces: get, update, start, stop, delete, rebuild, connect, ports | the owner, an owner of the owning agent, or a repo maintainer |
//! | export workspace changes | as above, plus `commit` on the workspace ref |
//! | prebuild configurations: write, trigger | `manage_settings` |
//! | dotfiles | the principal, or an owner of the agent |
//! | egress: set | org owner/admin (org), maintainer (repo), workspace owner (workspace) |
//! | usage | one's own usage (org member), or org owner/admin |

use std::sync::Arc;
use std::time::Duration;

use bee_policy::Action;
use bee_proto::v1::agent_service_server::{AgentService, AgentServiceServer};
use bee_proto::v1::workspace_service_server::{WorkspaceService, WorkspaceServiceServer};
use bee_proto::v1::*;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::Agents;
use super::registry::{
    ActivityFilter, AgentDefinition, DefinitionPatch, OwnerBudget, Schedule, run_repo, run_repo_ref,
};
use super::runs::{StartRun, Trigger, terminal};
use super::watchers::{NewAnomaly, NewWatcher, WatcherPatch};
use super::workspaces::NewWorkspace;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::service::events::reauthorize_every;
use crate::service::{ForgeServices, ResponseStream, next, page, reply, utf8_paths};
use crate::work::Actor;

/// `AgentService` and `WorkspaceService` over one [`Agents`].
#[derive(Clone)]
pub struct AgentApi {
    base: ForgeServices,
    agents: Arc<Agents>,
}

fn bearer<T>(req: &Request<T>) -> Option<String> {
    let v = req.metadata().get("authorization")?.to_str().ok()?;
    v.strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))
        .map(|t| t.trim().to_string())
}

fn ms(t: &Option<pbjson_types::Timestamp>) -> u64 {
    t.as_ref().map(ts_ms).unwrap_or(0)
}

impl AgentApi {
    /// The API over `agents`.
    pub fn new(agents: Arc<Agents>) -> AgentApi {
        AgentApi {
            base: ForgeServices::new(agents.forge.clone()),
            agents,
        }
    }

    /// The `AgentService` server.
    pub fn agent_server(&self) -> AgentServiceServer<AgentApi> {
        AgentServiceServer::new(self.clone())
    }

    /// The `WorkspaceService` server.
    pub fn workspace_server(&self) -> WorkspaceServiceServer<AgentApi> {
        WorkspaceServiceServer::new(self.clone())
    }

    fn caller<T>(&self, req: &Request<T>) -> std::result::Result<Caller, Status> {
        self.base.caller(req)
    }

    async fn repo(&self, caller: &Caller, r: &Option<RepoRef>) -> Result<Repo> {
        self.base.repo(caller, r).await
    }

    async fn read(&self, caller: &Caller, repo: &Repo, paths: &[String]) -> Result<Cx> {
        self.agents
            .forge
            .authz
            .repo(caller, repo, Action::Read, paths, None, None)
            .await
    }

    async fn write(&self, caller: &Caller, repo: &Repo, action: Action, audit: &str) -> Result<Cx> {
        self.write_on(caller, repo, action, None, audit).await
    }

    async fn write_on(
        &self,
        caller: &Caller,
        repo: &Repo,
        action: Action,
        ref_name: Option<&str>,
        audit: &str,
    ) -> Result<Cx> {
        let auth = self
            .agents
            .forge
            .authz
            .repo(caller, repo, action, &[], ref_name, None)
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

    async fn org(
        &self,
        caller: &Caller,
        org_id: &str,
        perm: OrgPerm,
        audit: Option<&str>,
    ) -> Result<Cx> {
        let auth = self.agents.forge.authz.org(caller, org_id, perm).await;
        match audit {
            Some(a) => self.base.guarded(caller, org_id, "", a, auth).await,
            None => auth,
        }
    }

    /// An agent principal by id or login.
    async fn agent_principal(&self, handle: &str) -> Result<Principal> {
        let h = handle.trim();
        if h.is_empty() {
            return Err(ForgeError::missing("agent"));
        }
        let dir = &self.agents.forge.hooks.directory;
        let p = match dir.get(h).await {
            Some(p) => Some(p),
            None => dir.by_login(h).await,
        };
        p.filter(|p| p.kind == PrincipalKind::Agent as i32)
            .ok_or_else(|| ForgeError::not_found(format!("agent {h}")))
    }

    /// The right to configure a feed trigger in `repo`: a repo maintainer (`manage_settings`
    /// at the maintain role) who is the agent or one of its owners, for an agent enabled in
    /// the repo's org.
    async fn feed_cx(&self, caller: &Caller, repo: &Repo, agent: &Principal) -> Result<Cx> {
        let audit = "agent.feed_trigger_set";
        let auth = self
            .agents
            .forge
            .authz
            .repo(
                caller,
                repo,
                Action::ManageSettings,
                &[],
                None,
                Some(RepoRole::Maintain),
            )
            .await;
        let cx = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, audit, auth)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        let def = self
            .agents
            .definition(&cx, &repo.org_id, &agent.id)
            .await
            .ok()
            .filter(|d| d.enabled)
            .ok_or_else(|| {
                ForgeError::invalid(
                    "no_agent",
                    format!("agent {} is not enabled in this org", agent.id),
                )
            })?;
        let me = &caller.cx.actor_id;
        if def.agent.id != *me && !def.owners.iter().any(|o| o == me) {
            return Err(self
                .deny(
                    caller,
                    &repo.org_id,
                    &repo.id,
                    audit,
                    format!("{me} does not own agent {}", agent.id),
                )
                .await);
        }
        Ok(cx)
    }

    /// The source repos of a feed trigger: each must exist and be readable by the agent
    /// (`INVALID_ARGUMENT`/`source_not_readable` otherwise, the same for both).
    async fn feed_sources(&self, agent: &Principal, sources: &[RepoRef]) -> Result<Vec<Repo>> {
        let acx =
            Cx::new(agent.id.clone(), PrincipalKind::Agent).with_chain(vec![agent.id.clone()]);
        let orgs = &self.agents.forge.orgs;
        let mut out = Vec::with_capacity(sources.len());
        for (i, r) in sources.iter().enumerate() {
            let bad = || {
                ForgeError::invalid(
                    "source_not_readable",
                    format!("sources[{i}] is not a repo agent {} can read", agent.id),
                )
            };
            let repo = orgs
                .resolve_repo(&Cx::system(), r)
                .await
                .map_err(|_| bad())?;
            if !crate::posts::adopt::chain_reads(orgs, &acx, &repo).await {
                return Err(bad());
            }
            out.push(repo);
        }
        Ok(out)
    }

    async fn org_id(&self, caller: &Caller, org: &str) -> Result<String> {
        if org.is_empty() {
            return Err(ForgeError::missing("org"));
        }
        self.base.org_id(caller, org).await
    }

    /// Whether the caller may drive an agent: it is the agent, one of its owners, or a
    /// maintainer of `repo`.
    async fn drives(&self, caller: &Caller, def: &AgentDefinition, repo: Option<&Repo>) -> bool {
        let me = &caller.cx.actor_id;
        if *me == def.agent.id || def.owners.iter().any(|o| o == me) {
            return true;
        }
        match repo {
            Some(r) => self
                .agents
                .forge
                .authz
                .repo(
                    caller,
                    r,
                    Action::ManageSettings,
                    &[],
                    None,
                    Some(RepoRole::Maintain),
                )
                .await
                .is_ok(),
            None => false,
        }
    }

    async fn deny(
        &self,
        caller: &Caller,
        org: &str,
        repo: &str,
        action: &str,
        msg: String,
    ) -> ForgeError {
        let e = ForgeError::permission_denied(msg);
        self.agents
            .forge
            .authz
            .audit_denial(caller, org, repo, action, &e)
            .await;
        e
    }

    /// The orgs where `agent` is defined and the caller is a member.
    async fn agent_orgs(&self, caller: &Caller, agent: &str) -> Result<(Principal, Vec<String>)> {
        let defs = self.agents.definitions_of(&caller.cx, agent).await?;
        let mut orgs = Vec::new();
        for d in &defs {
            if self
                .org(caller, &d.org_id, OrgPerm::ViewMembers, None)
                .await
                .is_ok()
            {
                orgs.push(d.org_id.clone());
            }
        }
        let p = defs
            .first()
            .map(|d| d.agent.clone())
            .filter(|_| !orgs.is_empty())
            .ok_or_else(|| ForgeError::not_found(format!("agent {agent}")))?;
        Ok((p, orgs))
    }

    async fn run_repo_and_rec(
        &self,
        caller: &Caller,
        id: &str,
    ) -> Result<(super::model::RunRec, Repo)> {
        if id.is_empty() {
            return Err(ForgeError::missing("id"));
        }
        let rec = self.agents.run_rec(&caller.cx, id).await?;
        let repo = self
            .agents
            .forge
            .orgs
            .repo_any(&caller.cx, &run_repo(&rec.run()))
            .await?;
        Ok((rec, repo))
    }

    /// Run mutations: `commit`, and being the starter, the agent, an owner or a maintainer.
    async fn steer_cx(
        &self,
        caller: &Caller,
        id: &str,
        audit: &str,
    ) -> Result<(Cx, super::model::RunRec, Repo)> {
        let (rec, repo) = self.run_repo_and_rec(caller, id).await?;
        let cx = self.write(caller, &repo, Action::Commit, audit).await?;
        let run = rec.run();
        let me = &caller.cx.actor_id;
        let starter = run.started_by.as_ref().is_some_and(|p| p.id == *me);
        let def = self
            .agents
            .definition(
                &caller.cx,
                &repo.org_id,
                &run.agent.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
            )
            .await
            .unwrap_or_default();
        if !starter && !self.drives(caller, &def, Some(&repo)).await {
            return Err(self
                .deny(
                    caller,
                    &repo.org_id,
                    &repo.id,
                    audit,
                    format!("{me} may not change run {id}"),
                )
                .await);
        }
        Ok((cx, rec, repo))
    }

    async fn memory_cx(
        &self,
        caller: &Caller,
        scope: &mut Option<AgentMemoryScope>,
        write: bool,
    ) -> Result<(Cx, String)> {
        let s = scope.clone().ok_or_else(|| ForgeError::missing("scope"))?;
        match s.scope {
            Some(agent_memory_scope::Scope::Repo(r)) => {
                let repo = self.repo(caller, &Some(r)).await?;
                *scope = Some(AgentMemoryScope {
                    scope: Some(agent_memory_scope::Scope::Repo(RepoRef {
                        repo: Some(repo_ref::Repo::Id(repo.id.clone())),
                    })),
                });
                let cx = if write {
                    self.write(caller, &repo, Action::Commit, "agent.memory_write")
                        .await?
                } else {
                    self.read(caller, &repo, &[]).await?
                };
                Ok((cx, repo.org_id))
            }
            Some(agent_memory_scope::Scope::Org(o)) => {
                let org = self.org_id(caller, &o).await?;
                *scope = Some(AgentMemoryScope {
                    scope: Some(agent_memory_scope::Scope::Org(org.clone())),
                });
                let cx = if write {
                    self.org(caller, &org, OrgPerm::Manage, Some("agent.memory_write"))
                        .await?
                } else {
                    self.org(caller, &org, OrgPerm::ViewMembers, None).await?
                };
                Ok((cx, org))
            }
            Some(agent_memory_scope::Scope::AgentPrincipalId(a)) => {
                let defs = self.agents.definitions_of(&caller.cx, &a).await?;
                let me = &caller.cx.actor_id;
                let ok = *me == a || defs.iter().any(|d| d.owners.iter().any(|o| o == me));
                let org = defs.first().map(|d| d.org_id.clone()).unwrap_or_default();
                if !ok {
                    if write {
                        return Err(self
                            .deny(
                                caller,
                                &org,
                                "",
                                "agent.memory_write",
                                format!("{me} does not own agent {a}"),
                            )
                            .await);
                    }
                    return Err(ForgeError::not_found(format!("agent {a}")));
                }
                let mut cx = caller.cx.clone();
                cx.decision = "agent:self-or-owner".into();
                Ok((cx, org))
            }
            None => Err(ForgeError::missing("scope")),
        }
    }

    async fn bundle_cx(
        &self,
        caller: &Caller,
        b: &ContextBundle,
        write: bool,
        audit: &str,
    ) -> Result<(Cx, String)> {
        let me = &caller.cx.actor_id;
        let owner = b.owner.as_ref().is_some_and(|o| o.id == *me);
        match b.repo.as_ref().filter(|r| r.repo.is_some()) {
            Some(r) => {
                let repo = self.repo(caller, &Some(r.clone())).await?;
                let cx = self.read(caller, &repo, &[]).await?;
                if !owner && !b.shared {
                    return Err(ForgeError::not_found(format!("context bundle {}", b.id)));
                }
                if write && !owner {
                    let cx = self
                        .write(caller, &repo, Action::ManageSettings, audit)
                        .await
                        .map_err(|_| {
                            ForgeError::permission_denied(
                                "only the owner or a maintainer may change this bundle",
                            )
                        })?;
                    return Ok((cx, repo.org_id));
                }
                Ok((cx, repo.org_id))
            }
            None => {
                let cx = self.org(caller, &b.org, OrgPerm::ViewMembers, None).await?;
                if !owner && !b.shared {
                    return Err(ForgeError::not_found(format!("context bundle {}", b.id)));
                }
                if write && !owner {
                    let cx = self
                        .org(caller, &b.org, OrgPerm::Manage, Some(audit))
                        .await?;
                    return Ok((cx, b.org.clone()));
                }
                Ok((cx, b.org.clone()))
            }
        }
    }

    /// Workspace access: the owner, an owner of the owning agent, or a repo maintainer.
    async fn ws_cx(
        &self,
        caller: &Caller,
        id: &str,
        audit: Option<&str>,
    ) -> Result<(Cx, Workspace, Repo)> {
        if id.is_empty() {
            return Err(ForgeError::missing("id"));
        }
        let (w, _) = self.agents.workspaces.get(&caller.cx, id).await?;
        let repo = self
            .agents
            .forge
            .orgs
            .repo_any(&caller.cx, &run_repo_ref(&w.repo))
            .await?;
        let cx = self.read(caller, &repo, &[]).await?;
        let me = &caller.cx.actor_id;
        let owner = w.owner.clone().unwrap_or_default();
        let mut ok = owner.id == *me;
        if !ok && owner.kind == PrincipalKind::Agent as i32 {
            ok = self
                .agents
                .definitions_of(&caller.cx, &owner.id)
                .await?
                .iter()
                .any(|d| d.owners.iter().any(|o| o == me));
        }
        if !ok {
            ok = self
                .agents
                .forge
                .authz
                .repo(
                    caller,
                    &repo,
                    Action::ManageSettings,
                    &[],
                    None,
                    Some(RepoRole::Maintain),
                )
                .await
                .is_ok();
        }
        if !ok {
            return Err(match audit {
                Some(a) => {
                    self.deny(
                        caller,
                        &repo.org_id,
                        &repo.id,
                        a,
                        format!("{me} may not use workspace {id}"),
                    )
                    .await
                }
                None => ForgeError::not_found(format!("workspace {id}")),
            });
        }
        if audit.is_some() && repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        Ok((cx, w, repo))
    }

    /// A principal the caller may act for: itself, or an agent it owns.
    async fn self_or_owned(&self, caller: &Caller, id: &str) -> Result<Principal> {
        let me = caller.cx.actor_id.clone();
        if id.is_empty() || id == me {
            return Ok(self
                .agents
                .forge
                .hooks
                .directory
                .get(&me)
                .await
                .unwrap_or_else(|| caller.principal()));
        }
        let owned = self
            .agents
            .definitions_of(&caller.cx, id)
            .await?
            .iter()
            .any(|d| d.owners.contains(&me));
        if !owned {
            return Err(ForgeError::permission_denied(format!(
                "{me} does not own {id}"
            )));
        }
        self.agents
            .forge
            .hooks
            .directory
            .get(id)
            .await
            .ok_or_else(|| ForgeError::not_found(format!("principal {id}")))
    }

    async fn egress_target(
        &self,
        caller: &Caller,
        scope: &Option<WorkspaceEgressScope>,
        write: bool,
    ) -> Result<(Cx, WorkspaceEgressScope, String, String)> {
        let s = scope.clone().ok_or_else(|| ForgeError::missing("scope"))?;
        match s.scope {
            Some(workspace_egress_scope::Scope::Org(o)) => {
                let org = self.org_id(caller, &o).await?;
                let cx = if write {
                    self.org(caller, &org, OrgPerm::Manage, Some("workspace.egress_set"))
                        .await?
                } else {
                    self.org(caller, &org, OrgPerm::ViewMembers, None).await?
                };
                Ok((
                    cx,
                    WorkspaceEgressScope {
                        scope: Some(workspace_egress_scope::Scope::Org(org.clone())),
                    },
                    org,
                    String::new(),
                ))
            }
            Some(workspace_egress_scope::Scope::Repo(r)) => {
                let repo = self.repo(caller, &Some(r)).await?;
                let cx = if write {
                    self.write(
                        caller,
                        &repo,
                        Action::ManageSettings,
                        "workspace.egress_set",
                    )
                    .await?
                } else {
                    self.read(caller, &repo, &[]).await?
                };
                Ok((
                    cx,
                    WorkspaceEgressScope {
                        scope: Some(workspace_egress_scope::Scope::Repo(RepoRef {
                            repo: Some(repo_ref::Repo::Id(repo.id.clone())),
                        })),
                    },
                    repo.org_id.clone(),
                    repo.id,
                ))
            }
            Some(workspace_egress_scope::Scope::WorkspaceId(w)) => {
                let (cx, ws, repo) = self
                    .ws_cx(caller, &w, write.then_some("workspace.egress_set"))
                    .await?;
                Ok((
                    cx,
                    WorkspaceEgressScope {
                        scope: Some(workspace_egress_scope::Scope::WorkspaceId(ws.id)),
                    },
                    repo.org_id,
                    repo.id,
                ))
            }
            None => Err(ForgeError::missing("scope")),
        }
    }
}

#[tonic::async_trait]
impl AgentService for AgentApi {
    async fn register_agent_runtime(
        &self,
        req: Request<RegisterAgentRuntimeRequest>,
    ) -> std::result::Result<Response<AgentRuntime>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let cx = self
                .org(
                    &caller,
                    &org,
                    OrgPerm::Manage,
                    Some("agent.runtime_register"),
                )
                .await?;
            self.agents
                .register_runtime(
                    &cx,
                    &org,
                    AgentRuntime {
                        name: r.name,
                        vendor: r.vendor,
                        version: r.version,
                        description: r.description,
                        capabilities: r.capabilities,
                        mcp_endpoint: r.mcp_endpoint,
                        image: r.image,
                        sandbox: r.sandbox,
                        default_models: r.default_models,
                        ..Default::default()
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent_runtime(
        &self,
        req: Request<GetAgentRuntimeRequest>,
    ) -> std::result::Result<Response<AgentRuntime>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let rt = self.agents.get_runtime(&caller.cx, &r.id).await?;
            if !rt.org.is_empty() {
                self.org(&caller, &rt.org, OrgPerm::ViewMembers, None)
                    .await
                    .map_err(|_| ForgeError::not_found(format!("runtime {}", r.id)))?;
            }
            Ok(rt)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_runtimes(
        &self,
        req: Request<ListAgentRuntimesRequest>,
    ) -> std::result::Result<Response<ListAgentRuntimesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = if r.org.is_empty() {
                String::new()
            } else {
                self.org_id(&caller, &r.org).await?
            };
            if !org.is_empty() {
                self.org(&caller, &org, OrgPerm::ViewMembers, None).await?;
            }
            let (after, size) = page(&r.page);
            let (runtimes, token) = self
                .agents
                .list_runtimes(
                    &caller.cx,
                    &org,
                    &r.capability,
                    r.include_public || org.is_empty(),
                    &after,
                    size,
                )
                .await?;
            Ok(ListAgentRuntimesResponse {
                runtimes,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_agent_runtime(
        &self,
        req: Request<UpdateAgentRuntimeRequest>,
    ) -> std::result::Result<Response<AgentRuntime>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let rt = self.agents.get_runtime(&caller.cx, &r.id).await?;
            if rt.org.is_empty() {
                return Err(ForgeError::permission_denied(
                    "public runtimes are managed by the operator",
                ));
            }
            let cx = self
                .org(
                    &caller,
                    &rt.org,
                    OrgPerm::Manage,
                    Some("agent.runtime_update"),
                )
                .await?;
            self.agents
                .update_runtime(
                    &cx,
                    &r.id,
                    r.version,
                    r.description,
                    r.capabilities,
                    r.mcp_endpoint,
                    r.image,
                    r.sandbox,
                    r.enabled,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_agent_runtime(
        &self,
        req: Request<DeleteAgentRuntimeRequest>,
    ) -> std::result::Result<Response<DeleteAgentRuntimeResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let rt = self.agents.get_runtime(&caller.cx, &r.id).await?;
            if rt.org.is_empty() {
                return Err(ForgeError::permission_denied(
                    "public runtimes are managed by the operator",
                ));
            }
            let cx = self
                .org(
                    &caller,
                    &rt.org,
                    OrgPerm::Manage,
                    Some("agent.runtime_delete"),
                )
                .await?;
            self.agents.delete_runtime(&cx, &r.id).await?;
            Ok(DeleteAgentRuntimeResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_principals(
        &self,
        req: Request<ListAgentPrincipalsRequest>,
    ) -> std::result::Result<Response<ListAgentPrincipalsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let cx = self.org(&caller, &org, OrgPerm::ViewMembers, None).await?;
            let all = self
                .agents
                .principal_summaries(
                    &cx,
                    &org,
                    &r.owner_id,
                    &r.runtime,
                    &r.principal_ids,
                    ms(&r.window_start),
                )
                .await?;
            let (after, size) = page(&r.page);
            let start: usize = after.parse().unwrap_or(0);
            let end = (start + size).min(all.len());
            let token = if end < all.len() {
                end.to_string()
            } else {
                String::new()
            };
            Ok(ListAgentPrincipalsResponse {
                agents: all
                    .into_iter()
                    .skip(start)
                    .take(end.saturating_sub(start))
                    .collect(),
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent_dashboard(
        &self,
        req: Request<GetAgentDashboardRequest>,
    ) -> std::result::Result<Response<AgentDashboard>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (agent, orgs) = self.agent_orgs(&caller, &r.principal_id).await?;
            if !r.repo_id.is_empty() {
                let repo = self
                    .agents
                    .forge
                    .orgs
                    .repo_any(&caller.cx, &r.repo_id)
                    .await?;
                self.read(&caller, &repo, &[]).await?;
            }
            let end = match ms(&r.window_end) {
                0 => u64::MAX,
                e => e,
            };
            self.agents
                .dashboard(
                    &caller.cx,
                    &agent,
                    &orgs,
                    ms(&r.window_start),
                    end,
                    &r.repo_id,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_work_items(
        &self,
        req: Request<ListAgentWorkItemsRequest>,
    ) -> std::result::Result<Response<ListAgentWorkItemsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (agent, orgs) = self.agent_orgs(&caller, &r.principal_id).await?;
            if !r.repo_id.is_empty() {
                let repo = self
                    .agents
                    .forge
                    .orgs
                    .repo_any(&caller.cx, &r.repo_id)
                    .await?;
                self.read(&caller, &repo, &[]).await?;
            }
            let (after, size) = page(&r.page);
            let (items, token) = self
                .agents
                .work_items(
                    &caller.cx,
                    &agent.id,
                    &orgs,
                    &r.repo_id,
                    ms(&r.since),
                    &after,
                    size,
                )
                .await?;
            Ok(ListAgentWorkItemsResponse {
                items,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn start_agent_run(
        &self,
        req: Request<StartAgentRunRequest>,
    ) -> std::result::Result<Response<AgentRun>, Status> {
        let caller = self.caller(&req)?;
        let token = bearer(&req);
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let task = r.task.clone().ok_or_else(|| ForgeError::missing("task"))?;
            let action = if task.kind == AgentTaskKind::Review as i32 {
                Action::Review
            } else {
                Action::Commit
            };
            let cx = self
                .write(&caller, &repo, action, "agent.run_start")
                .await?;
            let def = if r.agent_principal_id.is_empty() {
                self.agents
                    .default_agent(&cx, &repo.org_id, &caller.cx.actor_id)
                    .await?
            } else {
                self.agents
                    .definition(&cx, &repo.org_id, &r.agent_principal_id)
                    .await?
            };
            if !self.drives(&caller, &def, Some(&repo)).await {
                return Err(self
                    .deny(
                        &caller,
                        &repo.org_id,
                        &repo.id,
                        "agent.run_start",
                        format!(
                            "{} may not start runs of agent {}",
                            caller.cx.actor_id, def.name
                        ),
                    )
                    .await);
            }
            self.agents
                .start_run(
                    &cx,
                    StartRun {
                        repo,
                        task,
                        agent_id: def.agent.id.clone(),
                        runtime_id: r.runtime_id,
                        model: r.model,
                        base_ref: r.base_ref,
                        scope: r.scope.unwrap_or_default(),
                        budget: r.budget,
                        machine_type: r.machine_type,
                        idempotency_key: r.idempotency_key,
                        trigger: Trigger::Manual,
                        chain: caller.cx.chain.clone(),
                        parent_token: token,
                        ..StartRun::default()
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent_run(
        &self,
        req: Request<GetAgentRunRequest>,
    ) -> std::result::Result<Response<AgentRun>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (rec, repo) = self.run_repo_and_rec(&caller, &r.id).await?;
            self.read(&caller, &repo, &[]).await?;
            Ok(rec.run())
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_runs(
        &self,
        req: Request<ListAgentRunsRequest>,
    ) -> std::result::Result<Response<ListAgentRunsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repos: Vec<String> = if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                let repo = self.repo(&caller, &r.repo).await?;
                self.read(&caller, &repo, &[]).await?;
                vec![repo.id]
            } else {
                // Every repo the caller can read among the agent's runs.
                if r.agent_principal_id.is_empty() {
                    return Err(ForgeError::missing("repo"));
                }
                let (all, _) = self
                    .agents
                    .list_runs(
                        &caller.cx,
                        &self.all_run_repos(&r.agent_principal_id).await?,
                        &r.agent_principal_id,
                        &[],
                        0,
                        "",
                        "",
                        crate::store::MAX_SCAN,
                    )
                    .await?;
                let mut ids: Vec<String> = all.iter().map(run_repo).collect();
                ids.sort();
                ids.dedup();
                let mut ok = Vec::new();
                for id in ids {
                    if let Ok(repo) = self.agents.forge.orgs.repo_any(&caller.cx, &id).await
                        && self.read(&caller, &repo, &[]).await.is_ok()
                    {
                        ok.push(id);
                    }
                }
                ok
            };
            let (after, size) = page(&r.page);
            let (runs, token) = self
                .agents
                .list_runs(
                    &caller.cx,
                    &repos,
                    &r.agent_principal_id,
                    &r.states,
                    r.task_kind,
                    &r.started_by_id,
                    &after,
                    size,
                )
                .await?;
            Ok(ListAgentRunsResponse {
                runs,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_agent_run(
        &self,
        req: Request<CancelAgentRunRequest>,
    ) -> std::result::Result<Response<AgentRun>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self.steer_cx(&caller, &r.id, "agent.run_cancel").await?;
            self.agents.cancel_run(&cx, &r.id, &r.reason).await
        }
        .await;
        reply(&caller, out)
    }

    async fn retry_agent_run(
        &self,
        req: Request<RetryAgentRunRequest>,
    ) -> std::result::Result<Response<AgentRun>, Status> {
        let caller = self.caller(&req)?;
        let token = bearer(&req);
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self.steer_cx(&caller, &r.id, "agent.run_retry").await?;
            self.agents
                .retry_run(
                    &cx,
                    &r.id,
                    &r.runtime_id,
                    r.budget,
                    caller.cx.chain.clone(),
                    token,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    type StreamAgentRunEventsStream = ResponseStream<AgentRunEvent>;

    async fn stream_agent_run_events(
        &self,
        req: Request<StreamAgentRunEventsRequest>,
    ) -> std::result::Result<Response<Self::StreamAgentRunEventsStream>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, repo) = self.run_repo_and_rec(&caller, &r.run_id).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentRunEvent>>(64);
            let agents = self.agents.clone();
            let api = self.clone();
            let auth_caller = caller.clone();
            let repo = repo.clone();
            let id = r.run_id.clone();
            let kinds = r.kinds.clone();
            let mut after = r.after_seq;
            tokio::spawn(async move {
                let mut checked = tokio::time::Instant::now();
                loop {
                    if let Err(e) = reauthorize_every(&mut checked, || async {
                        let fresh = api.base.fresh(&auth_caller)?;
                        api.read(&fresh, &repo, &[]).await.map(|_| ())
                    })
                    .await
                    {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                    let notified = agents.run_done.notified();
                    // The request's deadline bounded opening the stream, not the stream.
                    let cx = cx.clone().with_timeout(crate::cx::DEFAULT_DEADLINE);
                    let events = match agents.run_events(&cx, &id, after, &[], 500).await {
                        Ok(e) => e,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                    let empty = events.is_empty();
                    for e in events {
                        after = e.seq;
                        if (kinds.is_empty() || kinds.contains(&e.kind))
                            && tx.send(Ok(e)).await.is_err()
                        {
                            return;
                        }
                    }
                    if empty {
                        match agents.get_run(&cx, &id).await {
                            Ok(run) if terminal(run.state) && run.last_event_seq <= after => return,
                            Ok(_) => {}
                            Err(e) => {
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                        }
                        if tx.is_closed() {
                            return;
                        }
                        let _ = tokio::time::timeout(Duration::from_millis(500), notified).await;
                    }
                }
            });
            let rid = caller.cx.request_id.clone();
            let stream = ReceiverStream::new(rx).map(move |i| i.map_err(|e| e.to_status(&rid)));
            Ok(Box::pin(stream) as ResponseStream<AgentRunEvent>)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_run_events(
        &self,
        req: Request<ListAgentRunEventsRequest>,
    ) -> std::result::Result<Response<ListAgentRunEventsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, repo) = self.run_repo_and_rec(&caller, &r.run_id).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (token, size) = page(&r.page);
            let after = if token.is_empty() {
                r.after_seq
            } else {
                token.parse().unwrap_or(r.after_seq)
            };
            let events = self
                .agents
                .run_events(&cx, &r.run_id, after, &r.kinds, size + 1)
                .await?;
            let more = events.len() > size;
            let events: Vec<AgentRunEvent> = events.into_iter().take(size).collect();
            let token = if more {
                events.last().map(|e| e.seq.to_string()).unwrap_or_default()
            } else {
                String::new()
            };
            Ok(ListAgentRunEventsResponse {
                events,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn send_agent_run_message(
        &self,
        req: Request<SendAgentRunMessageRequest>,
    ) -> std::result::Result<Response<AgentRunEvent>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self
                .steer_cx(&caller, &r.run_id, "agent.run_message")
                .await?;
            self.agents.send_message(&cx, &r.run_id, &r.body).await
        }
        .await;
        reply(&caller, out)
    }

    async fn resolve_agent_instructions(
        &self,
        req: Request<ResolveAgentInstructionsRequest>,
    ) -> std::result::Result<Response<ResolveAgentInstructionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let paths = utf8_paths(&r.paths)?;
            let cx = self.read(&caller, &repo, &paths).await?;
            let at = self
                .agents
                .ports
                .repo
                .resolve(&repo, &r.revision.map(|v| v.expr).unwrap_or_default())
                .await?;
            let res = self
                .agents
                .resolve_instructions(&cx, &repo, &at, &paths, &r.runtime, &r.custom_agent)
                .await?;
            Ok(ResolveAgentInstructionsResponse {
                files: res.files,
                combined: res.combined,
                changeset: Some(res.changeset),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_instruction_files(
        &self,
        req: Request<ListAgentInstructionFilesRequest>,
    ) -> std::result::Result<Response<ListAgentInstructionFilesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            self.read(&caller, &repo, &[]).await?;
            let at = self
                .agents
                .ports
                .repo
                .resolve(&repo, &r.revision.map(|v| v.expr).unwrap_or_default())
                .await?;
            let files = self
                .agents
                .list_instruction_files(&repo, &at, r.include_content)
                .await?;
            Ok(ListAgentInstructionFilesResponse { files })
        }
        .await;
        reply(&caller, out)
    }

    async fn put_agent_memory(
        &self,
        req: Request<PutAgentMemoryRequest>,
    ) -> std::result::Result<Response<AgentMemory>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut scope = r.scope;
            let (cx, org) = self.memory_cx(&caller, &mut scope, true).await?;
            self.agents
                .put_memory(
                    &cx,
                    &org,
                    AgentMemory {
                        scope,
                        key: r.key,
                        value: r.value,
                        content_type: r.content_type,
                        tags: r.tags,
                        sources: r.sources,
                        ..Default::default()
                    },
                    r.expected_version,
                    r.ttl_seconds,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent_memory(
        &self,
        req: Request<GetAgentMemoryRequest>,
    ) -> std::result::Result<Response<AgentMemory>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut scope = r.scope;
            let (cx, _) = self.memory_cx(&caller, &mut scope, false).await?;
            self.agents
                .get_memory(&cx, &scope.unwrap_or_default(), &r.key)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_memories(
        &self,
        req: Request<ListAgentMemoriesRequest>,
    ) -> std::result::Result<Response<ListAgentMemoriesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut scope = r.scope;
            let (cx, _) = self.memory_cx(&caller, &mut scope, false).await?;
            let (after, size) = page(&r.page);
            let (memories, token) = self
                .agents
                .list_memories(
                    &cx,
                    &scope.unwrap_or_default(),
                    &r.key_prefix,
                    &r.tags,
                    &after,
                    size,
                )
                .await?;
            Ok(ListAgentMemoriesResponse {
                memories,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_agent_memory(
        &self,
        req: Request<DeleteAgentMemoryRequest>,
    ) -> std::result::Result<Response<DeleteAgentMemoryResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut scope = r.scope;
            let (cx, org) = self.memory_cx(&caller, &mut scope, true).await?;
            self.agents
                .delete_memory(&cx, &org, &scope.unwrap_or_default(), &r.key)
                .await?;
            Ok(DeleteAgentMemoryResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn search_agent_memories(
        &self,
        req: Request<SearchAgentMemoriesRequest>,
    ) -> std::result::Result<Response<SearchAgentMemoriesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let mut scope = r.scope;
            let (cx, _) = self.memory_cx(&caller, &mut scope, false).await?;
            let limit = if r.limit == 0 { 10 } else { r.limit as usize };
            let hits = self
                .agents
                .search_memories(&cx, &scope.unwrap_or_default(), &r.query, &r.tags, limit)
                .await?;
            Ok(SearchAgentMemoriesResponse {
                hits: hits
                    .into_iter()
                    .map(|(memory, score)| AgentMemoryHit {
                        memory: Some(memory),
                        score,
                    })
                    .collect(),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_context_bundle(
        &self,
        req: Request<CreateContextBundleRequest>,
    ) -> std::result::Result<Response<ContextBundle>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org, repo_ref, org_name) = if r.repo.as_ref().is_some_and(|x| x.repo.is_some())
            {
                let repo = self.repo(&caller, &r.repo).await?;
                let auth = self.read(&caller, &repo, &[]).await;
                let cx = self
                    .base
                    .guarded(&caller, &repo.org_id, &repo.id, "agent.bundle_create", auth)
                    .await?;
                (
                    cx,
                    repo.org_id.clone(),
                    super::registry::repo_ref_id(&repo.id),
                    String::new(),
                )
            } else {
                let org = self.org_id(&caller, &r.org).await?;
                let cx = self
                    .org(
                        &caller,
                        &org,
                        OrgPerm::ViewMembers,
                        Some("agent.bundle_create"),
                    )
                    .await?;
                (cx, org.clone(), None, org)
            };
            self.agents
                .create_bundle(
                    &cx,
                    &org,
                    ContextBundle {
                        repo: repo_ref,
                        org: org_name,
                        name: r.name,
                        description: r.description,
                        items: r.items,
                        shared: r.shared,
                        ..Default::default()
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_context_bundle(
        &self,
        req: Request<GetContextBundleRequest>,
    ) -> std::result::Result<Response<ContextBundle>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let b = self.agents.get_bundle(&caller.cx, &r.id).await?;
            self.bundle_cx(&caller, &b, false, "").await?;
            Ok(b)
        }
        .await;
        reply(&caller, out)
    }

    async fn list_context_bundles(
        &self,
        req: Request<ListContextBundlesRequest>,
    ) -> std::result::Result<Response<ListContextBundlesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, repo_id, org_id) = if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                let repo = self.repo(&caller, &r.repo).await?;
                let cx = self.read(&caller, &repo, &[]).await?;
                (cx, repo.id, String::new())
            } else {
                let org = self.org_id(&caller, &r.org).await?;
                let cx = self.org(&caller, &org, OrgPerm::ViewMembers, None).await?;
                (cx, String::new(), org)
            };
            let (after, size) = page(&r.page);
            let (bundles, token) = self
                .agents
                .list_bundles(
                    &cx,
                    &repo_id,
                    &org_id,
                    &r.owner_id,
                    &caller.cx.actor_id,
                    &after,
                    size,
                )
                .await?;
            Ok(ListContextBundlesResponse {
                bundles,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_context_bundle(
        &self,
        req: Request<UpdateContextBundleRequest>,
    ) -> std::result::Result<Response<ContextBundle>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let b = self.agents.get_bundle(&caller.cx, &r.id).await?;
            let (cx, org) = self
                .bundle_cx(&caller, &b, true, "agent.bundle_update")
                .await?;
            self.agents
                .update_bundle(&cx, &org, &r.id, r.name, r.description, r.items, r.shared)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_context_bundle(
        &self,
        req: Request<DeleteContextBundleRequest>,
    ) -> std::result::Result<Response<DeleteContextBundleResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let b = self.agents.get_bundle(&caller.cx, &r.id).await?;
            let (cx, org) = self
                .bundle_cx(&caller, &b, true, "agent.bundle_delete")
                .await?;
            self.agents.delete_bundle(&cx, &org, &r.id).await?;
            Ok(DeleteContextBundleResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn materialize_context_bundle(
        &self,
        req: Request<MaterializeContextBundleRequest>,
    ) -> std::result::Result<Response<MaterializeContextBundleResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let b = self.agents.get_bundle(&caller.cx, &r.id).await?;
            let (cx, _) = self.bundle_cx(&caller, &b, false, "").await?;
            let rev = r.revision.map(|v| v.expr);
            let mut m = self
                .agents
                .materialize_bundle(&cx, &r.id, rev.as_deref(), r.include_bodies, r.max_files)
                .await?;
            // Items naming other repos are shown only when the caller can read them.
            let own = run_repo_ref(&b.repo);
            for it in &mut m.items {
                if let Some(context_bundle_item::Item::Subject(s)) =
                    it.item.as_ref().and_then(|i| i.item.as_ref())
                    && s.repo.as_ref().is_some_and(|x| x.repo.is_some())
                {
                    let ok = match self
                        .agents
                        .forge
                        .orgs
                        .resolve_repo(&caller.cx, s.repo.as_ref().unwrap_or(&RepoRef::default()))
                        .await
                    {
                        Ok(repo) if repo.id == own => true,
                        Ok(repo) => self.read(&caller, &repo, &[]).await.is_ok(),
                        Err(_) => false,
                    };
                    if !ok {
                        it.status = ItemStatus::PermissionDenied as i32;
                        it.title.clear();
                        it.body.clear();
                    }
                }
            }
            Ok(m)
        }
        .await;
        reply(&caller, out)
    }

    async fn create_component_watcher(
        &self,
        req: Request<CreateComponentWatcherRequest>,
    ) -> std::result::Result<Response<Watcher>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "agent.watcher_create",
                )
                .await?;
            // The watching agent: one of the caller's agents in this org (matching the runtime
            // when one is named).
            let defs = self.agents.definitions(&cx, &repo.org_id).await?;
            let me = &caller.cx.actor_id;
            let drives = |d: &&AgentDefinition| {
                d.enabled && (d.owners.iter().any(|o| o == me) || d.agent.id == *me)
            };
            let def = if r.agent_principal_id.is_empty() {
                defs.iter()
                    .filter(drives)
                    .find(|d| {
                        r.runtime_id.is_empty()
                            || d.runtime_id == r.runtime_id
                            || d.runtime == r.runtime_id
                    })
                    .ok_or_else(|| {
                        ForgeError::invalid(
                            "no_agent",
                            format!("{me} owns no enabled agent in this org to watch with"),
                        )
                    })?
            } else {
                let named = defs
                    .iter()
                    .find(|d| d.agent.id == r.agent_principal_id)
                    .filter(|d| d.enabled)
                    .ok_or_else(|| {
                        ForgeError::invalid(
                            "no_agent",
                            format!("agent {} is not enabled in this org", r.agent_principal_id),
                        )
                    })?;
                if !drives(&named) {
                    return Err(self
                        .deny(
                            &caller,
                            &repo.org_id,
                            &repo.id,
                            "agent.watcher_create",
                            format!("{me} does not own agent {}", r.agent_principal_id),
                        )
                        .await);
                }
                named
            };
            self.agents
                .create_watcher(
                    &cx,
                    &repo,
                    NewWatcher {
                        name: r.name,
                        component: r.component,
                        paths: r.paths,
                        service_name: r.service_name,
                        telemetry: r.telemetry,
                        runtime_id: if r.runtime_id.is_empty() {
                            def.runtime_id.clone()
                        } else {
                            r.runtime_id
                        },
                        agent: def.agent.clone(),
                        instructions: r.instructions,
                        auto_open_issues: r.auto_open_issues,
                        labels: r.labels,
                        check_interval_seconds: r.check_interval_seconds,
                        budget: r.budget,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_component_watcher(
        &self,
        req: Request<GetComponentWatcherRequest>,
    ) -> std::result::Result<Response<Watcher>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            self.agents.get_watcher(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_component_watchers(
        &self,
        req: Request<ListComponentWatchersRequest>,
    ) -> std::result::Result<Response<ListComponentWatchersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (after, size) = page(&r.page);
            let (watchers, token) = self
                .agents
                .list_watchers(&cx, &repo, r.state, &after, size)
                .await?;
            Ok(ListComponentWatchersResponse {
                watchers,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_component_watcher(
        &self,
        req: Request<UpdateComponentWatcherRequest>,
    ) -> std::result::Result<Response<Watcher>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "agent.watcher_update",
                )
                .await?;
            self.agents
                .update_watcher(
                    &cx,
                    &repo,
                    &r.id,
                    WatcherPatch {
                        name: r.name,
                        paths: r.paths,
                        service_name: r.service_name,
                        telemetry: r.telemetry,
                        state: r.state.map(|s| {
                            WatcherState::try_from(s).unwrap_or(WatcherState::Unspecified)
                        }),
                        instructions: r.instructions,
                        auto_open_issues: r.auto_open_issues,
                        check_interval_seconds: r.check_interval_seconds,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_component_watcher(
        &self,
        req: Request<DeleteComponentWatcherRequest>,
    ) -> std::result::Result<Response<DeleteComponentWatcherResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "agent.watcher_delete",
                )
                .await?;
            self.agents.delete_watcher(&cx, &repo, &r.id).await?;
            Ok(DeleteComponentWatcherResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn report_watcher_anomaly(
        &self,
        req: Request<ReportWatcherAnomalyRequest>,
    ) -> std::result::Result<Response<Anomaly>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let w = {
                let cx = self.read(&caller, &repo, &[]).await?;
                self.agents.get_watcher(&cx, &repo, &r.watcher_id).await?
            };
            let is_agent = w.agent.as_ref().is_some_and(|a| a.id == caller.cx.actor_id);
            let cx = if is_agent {
                let auth = self.read(&caller, &repo, &[]).await;
                self.base
                    .guarded(
                        &caller,
                        &repo.org_id,
                        &repo.id,
                        "agent.anomaly_report",
                        auth,
                    )
                    .await?
            } else {
                self.write(&caller, &repo, Action::Commit, "agent.anomaly_report")
                    .await?
            };
            self.agents
                .report_anomaly(
                    &cx,
                    &repo,
                    NewAnomaly {
                        watcher_id: r.watcher_id,
                        title: r.title,
                        description: r.description,
                        severity: AnomalySeverity::try_from(r.severity)
                            .unwrap_or(AnomalySeverity::Unspecified),
                        evidence: r.evidence,
                        started_ms: ms(&r.started_at),
                        fingerprint: r.fingerprint,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_watcher_anomalies(
        &self,
        req: Request<ListWatcherAnomaliesRequest>,
    ) -> std::result::Result<Response<ListWatcherAnomaliesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (after, size) = page(&r.page);
            let (anomalies, token) = self
                .agents
                .list_anomalies(
                    &cx,
                    &repo,
                    &r.watcher_id,
                    &r.states,
                    r.min_severity,
                    ms(&r.since),
                    &after,
                    size,
                )
                .await?;
            Ok(ListWatcherAnomaliesResponse {
                anomalies,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_watcher_anomaly(
        &self,
        req: Request<GetWatcherAnomalyRequest>,
    ) -> std::result::Result<Response<Anomaly>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            self.agents.get_anomaly(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    type StreamWatcherAnomaliesStream = ResponseStream<Anomaly>;

    async fn stream_watcher_anomalies(
        &self,
        req: Request<StreamWatcherAnomaliesRequest>,
    ) -> std::result::Result<Response<Self::StreamWatcherAnomaliesStream>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Anomaly>>(64);
            let agents = self.agents.clone();
            let mut after = r.after_seq;
            let (watcher, min) = (r.watcher_id.clone(), r.min_severity);
            tokio::spawn(async move {
                loop {
                    let notified = agents.run_done.notified();
                    match agents
                        .anomalies_after(&cx, &repo.id, &watcher, after, min)
                        .await
                    {
                        Ok(list) => {
                            for a in list {
                                after = after.max(a.seq);
                                if tx.send(Ok(a)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                    if tx.is_closed() {
                        return;
                    }
                    let _ = tokio::time::timeout(Duration::from_millis(500), notified).await;
                }
            });
            let rid = caller.cx.request_id.clone();
            let stream = ReceiverStream::new(rx).map(move |i| i.map_err(|e| e.to_status(&rid)));
            Ok(Box::pin(stream) as ResponseStream<Anomaly>)
        }
        .await;
        reply(&caller, out)
    }

    async fn acknowledge_watcher_anomaly(
        &self,
        req: Request<AcknowledgeWatcherAnomalyRequest>,
    ) -> std::result::Result<Response<Anomaly>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(&caller, &repo, Action::Triage, "agent.anomaly_acknowledge")
                .await?;
            self.agents
                .acknowledge_anomaly(&cx, &repo, &r.id, &r.note, r.resolve)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn link_watcher_anomaly_to_issue(
        &self,
        req: Request<LinkWatcherAnomalyToIssueRequest>,
    ) -> std::result::Result<Response<Anomaly>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(&caller, &repo, Action::Triage, "agent.anomaly_link_issue")
                .await?;
            let number = if r.issue.trim().is_empty() {
                0
            } else {
                r.issue
                    .trim()
                    .trim_start_matches('#')
                    .parse::<u64>()
                    .map_err(|_| ForgeError::invalid("invalid_issue", "issue is a number"))?
            };
            let principal = self
                .agents
                .forge
                .hooks
                .directory
                .get(&caller.cx.actor_id)
                .await
                .unwrap_or_else(|| caller.principal());
            let actor = Actor {
                principal,
                triage: true,
                admin: false,
                association: "MEMBER".into(),
            };
            self.agents
                .link_anomaly(&cx, &repo, &actor, &r.id, number, &r.title, &r.labels)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent_review_config(
        &self,
        req: Request<GetAgentReviewConfigRequest>,
    ) -> std::result::Result<Response<AgentReviewConfig>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            self.agents.review_config(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_agent_review_config(
        &self,
        req: Request<SetAgentReviewConfigRequest>,
    ) -> std::result::Result<Response<AgentReviewConfig>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "agent.review_config_set",
                )
                .await?;
            self.agents
                .set_review_config(
                    &cx,
                    &repo,
                    r.trigger.map(|t| {
                        AgentReviewTrigger::try_from(t).unwrap_or(AgentReviewTrigger::Unspecified)
                    }),
                    r.runtime_id,
                    r.agent_principal_id,
                    r.paths,
                    r.include_drafts,
                    r.review_agent_authored,
                    r.instructions,
                    r.budget,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_agent_feed_trigger(
        &self,
        req: Request<SetAgentFeedTriggerRequest>,
    ) -> std::result::Result<Response<AgentFeedTrigger>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let agent = self.agent_principal(&r.agent).await?;
            let cx = self.feed_cx(&caller, &repo, &agent).await?;
            if r.sources.len() > super::feed::MAX_SOURCES {
                return Err(ForgeError::too_large(
                    "sources",
                    super::feed::MAX_SOURCES as u64,
                    r.sources.len() as u64,
                ));
            }
            let sources = self.feed_sources(&agent, &r.sources).await?;
            self.agents
                .set_feed_trigger(
                    &cx,
                    &repo,
                    &agent,
                    super::feed::FeedSettings {
                        enabled: r.enabled,
                        daily_usd_budget: r.daily_usd_budget,
                        max_runs_per_day: r.max_runs_per_day,
                        sources,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent_feed_trigger(
        &self,
        req: Request<GetAgentFeedTriggerRequest>,
    ) -> std::result::Result<Response<AgentFeedTrigger>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let agent = self.agent_principal(&r.agent).await?;
            self.agents.get_feed_trigger(&cx, &repo, &agent.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_feed_triggers(
        &self,
        req: Request<ListAgentFeedTriggersRequest>,
    ) -> std::result::Result<Response<ListAgentFeedTriggersResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (token, limit) = page(&r.page);
            let (triggers, nt) = self
                .agents
                .list_feed_triggers(&cx, &repo, &token, limit)
                .await?;
            Ok(ListAgentFeedTriggersResponse {
                triggers,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn register_agent(
        &self,
        req: Request<RegisterAgentRequest>,
    ) -> std::result::Result<Response<bee_proto::v1::AgentDefinition>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            if r.agent_principal_id.is_empty() {
                return Err(ForgeError::missing("agent_principal_id"));
            }
            let agent = self
                .agents
                .forge
                .hooks
                .directory
                .get(&r.agent_principal_id)
                .await
                .ok_or_else(|| {
                    ForgeError::not_found(format!("principal {}", r.agent_principal_id))
                })?;
            let cx = self
                .definition_cx(
                    &caller,
                    &org,
                    std::slice::from_ref(&agent.owner_id),
                    "agent.define",
                )
                .await?;
            let runtime = if r.runtime.is_empty() && r.runtime_id.is_empty() {
                agent.runtime.clone()
            } else {
                r.runtime
            };
            let def = self
                .agents
                .define_agent(
                    &cx,
                    AgentDefinition {
                        org_id: org,
                        agent,
                        name: r.name,
                        runtime_id: r.runtime_id,
                        runtime,
                        model: r.model,
                        instructions_path: r.instructions_path,
                        toolsets: r.toolsets,
                        default_budget: r.default_budget.unwrap_or_default(),
                        owners: r.owners,
                        purpose: r.purpose,
                        enabled: r.enabled.unwrap_or(true),
                        schedules: schedules_from_wire(r.schedules),
                        created_ms: 0,
                        updated_ms: 0,
                    },
                )
                .await?;
            Ok(wire_definition(def))
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent(
        &self,
        req: Request<GetAgentRequest>,
    ) -> std::result::Result<Response<bee_proto::v1::AgentDefinition>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let cx = self.org(&caller, &org, OrgPerm::ViewMembers, None).await?;
            if r.agent_principal_id.is_empty() {
                return Err(ForgeError::missing("agent_principal_id"));
            }
            let def = self
                .agents
                .definition(&cx, &org, &r.agent_principal_id)
                .await?;
            Ok(wire_definition(def))
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agents(
        &self,
        req: Request<ListAgentsRequest>,
    ) -> std::result::Result<Response<ListAgentsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let cx = self.org(&caller, &org, OrgPerm::ViewMembers, None).await?;
            let (after, size) = page(&r.page);
            let skip: usize = if after.is_empty() {
                0
            } else {
                after.parse().map_err(|_| {
                    ForgeError::invalid("invalid_page_token", "malformed page token")
                })?
            };
            let defs: Vec<AgentDefinition> = self
                .agents
                .definitions(&cx, &org)
                .await?
                .into_iter()
                .filter(|d| r.owner_id.is_empty() || d.owners.contains(&r.owner_id))
                .collect();
            let end = skip.saturating_add(size).min(defs.len());
            let token = if end < defs.len() {
                end.to_string()
            } else {
                String::new()
            };
            Ok(ListAgentsResponse {
                agents: defs
                    .into_iter()
                    .skip(skip)
                    .take(size)
                    .map(wire_definition)
                    .collect(),
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_agent(
        &self,
        req: Request<UpdateAgentRequest>,
    ) -> std::result::Result<Response<bee_proto::v1::AgentDefinition>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            if r.agent_principal_id.is_empty() {
                return Err(ForgeError::missing("agent_principal_id"));
            }
            let current = self
                .agents
                .definition(&caller.cx, &org, &r.agent_principal_id)
                .await?;
            let cx = self
                .definition_cx(&caller, &org, &current.owners, "agent.update")
                .await?;
            let patch = DefinitionPatch {
                name: r.name,
                runtime_id: r.runtime_id,
                runtime: r.runtime,
                model: r.model,
                instructions_path: r.instructions_path,
                toolsets: (r.replace_toolsets || !r.toolsets.is_empty()).then_some(r.toolsets),
                default_budget: r.default_budget,
                owners: (!r.owners.is_empty()).then_some(r.owners),
                purpose: r.purpose,
                enabled: r.enabled,
                schedules: (r.replace_schedules || !r.schedules.is_empty())
                    .then(|| schedules_from_wire(r.schedules)),
            };
            let def = self
                .agents
                .update_agent(&cx, &org, &r.agent_principal_id, patch)
                .await?;
            Ok(wire_definition(def))
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_agent(
        &self,
        req: Request<DeleteAgentRequest>,
    ) -> std::result::Result<Response<DeleteAgentResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            if r.agent_principal_id.is_empty() {
                return Err(ForgeError::missing("agent_principal_id"));
            }
            let current = self
                .agents
                .definition(&caller.cx, &org, &r.agent_principal_id)
                .await?;
            let cx = self
                .definition_cx(&caller, &org, &current.owners, "agent.delete")
                .await?;
            self.agents
                .delete_agent(&cx, &org, &r.agent_principal_id)
                .await?;
            Ok(DeleteAgentResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn set_agent_budget(
        &self,
        req: Request<SetAgentBudgetRequest>,
    ) -> std::result::Result<Response<AgentOwnerBudget>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let cx = self
                .org(&caller, &org, OrgPerm::Manage, Some("agent.budget_set"))
                .await?;
            let owner = if r.owner_id.is_empty() {
                caller.cx.actor_id.clone()
            } else {
                r.owner_id
            };
            self.agents
                .set_owner_budget(
                    &cx,
                    &org,
                    &owner,
                    OwnerBudget {
                        max_usd_micros: r.max_usd_micros,
                        max_input_tokens: r.max_input_tokens,
                        max_output_tokens: r.max_output_tokens,
                        max_runs: r.max_runs,
                    },
                )
                .await?;
            self.budget_reply(&cx, &org, &owner).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_agent_budget(
        &self,
        req: Request<GetAgentBudgetRequest>,
    ) -> std::result::Result<Response<AgentOwnerBudget>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let me = caller.cx.actor_id.clone();
            let owner = if r.owner_id.is_empty() {
                me.clone()
            } else {
                r.owner_id
            };
            let cx = if owner == me {
                self.org(&caller, &org, OrgPerm::ViewMembers, None).await?
            } else {
                self.org(&caller, &org, OrgPerm::Manage, None).await?
            };
            self.budget_reply(&cx, &org, &owner).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_sessions_for_change(
        &self,
        req: Request<GetSessionsForChangeRequest>,
    ) -> std::result::Result<Response<GetSessionsForChangeResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let changeset = r.changeset.filter(|c| !c.raw.is_empty());
            let cr = r.change_request.trim().trim_start_matches('#').to_string();
            if changeset.is_none() && cr.is_empty() {
                return Err(ForgeError::missing("changeset"));
            }
            let cr_id = if cr.is_empty() {
                None
            } else if let Ok(n) = cr.parse::<u64>() {
                Some(self.agents.changes.get_by_number(&cx, &repo, n).await?.id)
            } else {
                Some(cr)
            };
            let sessions = self
                .agents
                .sessions_for_change(&cx, &repo.id, changeset.as_ref(), cr_id.as_deref())
                .await?;
            Ok(GetSessionsForChangeResponse {
                sessions: sessions
                    .into_iter()
                    .map(|s| AgentSession {
                        session: s.session,
                        run_id: s.run_id,
                        agent_principal_id: s.agent_id,
                        model: s.model,
                        runtime: s.runtime,
                        prompt_digest: s.prompt_digest,
                        chain: s.chain,
                        changesets: s.changesets,
                        change_request_ids: s.change_request_ids,
                        cost: Some(s.cost),
                        created_at: Some(crate::ids::ts(s.created_ms)),
                    })
                    .collect(),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_agent_activity(
        &self,
        req: Request<ListAgentActivityRequest>,
    ) -> std::result::Result<Response<ListAgentActivityResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let cx = self.org(&caller, &org, OrgPerm::ViewMembers, None).await?;
            let (events, next_seq) = self
                .agents
                .activity(
                    &cx,
                    &ActivityFilter {
                        org_id: org,
                        agent_id: r.agent_principal_id,
                        delegation_root: r.delegation_root,
                        kinds: r.kinds,
                        from_seq: r.from_seq,
                        limit: r.limit as usize,
                    },
                )
                .await?;
            // Events of repos the caller cannot read are dropped.
            let mut readable: std::collections::BTreeMap<String, bool> = Default::default();
            let mut out = Vec::with_capacity(events.len());
            for e in events {
                if !e.repo_id.is_empty() {
                    let ok = match readable.get(&e.repo_id) {
                        Some(ok) => *ok,
                        None => {
                            let ok = match self
                                .agents
                                .forge
                                .orgs
                                .repo_any(&caller.cx, &e.repo_id)
                                .await
                            {
                                Ok(repo) => self.read(&caller, &repo, &[]).await.is_ok(),
                                Err(_) => false,
                            };
                            readable.insert(e.repo_id.clone(), ok);
                            ok
                        }
                    };
                    if !ok {
                        continue;
                    }
                }
                out.push(e);
            }
            Ok(ListAgentActivityResponse {
                events: out,
                next_seq,
            })
        }
        .await;
        reply(&caller, out)
    }
}

impl AgentApi {
    /// Agent definition writes: an owner of the agent who is an org member, or an org owner
    /// or admin. Denials are audited.
    async fn definition_cx(
        &self,
        caller: &Caller,
        org: &str,
        owners: &[String],
        audit: &str,
    ) -> Result<Cx> {
        let me = &caller.cx.actor_id;
        if owners.iter().any(|o| o == me) {
            let mut cx = self
                .org(caller, org, OrgPerm::ViewMembers, Some(audit))
                .await?;
            cx.decision = "agent:owner".into();
            return Ok(cx);
        }
        self.org(caller, org, OrgPerm::Manage, Some(audit)).await
    }

    /// An owner's budget and spend as the wire message.
    async fn budget_reply(&self, cx: &Cx, org: &str, owner: &str) -> Result<AgentOwnerBudget> {
        let (b, s) = self.agents.owner_budget(cx, org, owner).await?;
        Ok(AgentOwnerBudget {
            org: org.to_string(),
            owner_id: owner.to_string(),
            max_usd_micros: b.max_usd_micros,
            max_input_tokens: b.max_input_tokens,
            max_output_tokens: b.max_output_tokens,
            max_runs: b.max_runs,
            spent: Some(Cost {
                input_tokens: s.input_tokens,
                output_tokens: s.output_tokens,
                usd_micros: s.usd_micros,
            }),
            runs_started: s.runs,
            exhausted: b.exhausted_by(&s).unwrap_or("").to_string(),
        })
    }
    async fn all_run_repos(&self, agent: &str) -> Result<Vec<String>> {
        let facts = self.agents.facts(&Cx::system(), agent, "").await?;
        let mut v: Vec<String> = facts.runs.iter().map(|r| run_repo(&r.run())).collect();
        v.sort();
        v.dedup();
        Ok(v)
    }
}

#[tonic::async_trait]
impl WorkspaceService for AgentApi {
    async fn list_workspace_machine_types(
        &self,
        req: Request<ListWorkspaceMachineTypesRequest>,
    ) -> std::result::Result<Response<ListWorkspaceMachineTypesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, org, repo) = if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                let repo = self.repo(&caller, &r.repo).await?;
                let cx = self.read(&caller, &repo, &[]).await?;
                (cx, repo.org_id.clone(), Some(repo))
            } else {
                let org = self.org_id(&caller, &r.org).await?;
                (
                    self.org(&caller, &org, OrgPerm::ViewMembers, None).await?,
                    org,
                    None,
                )
            };
            let machine_types = self
                .agents
                .workspaces
                .machine_types(&cx, &org, repo.as_ref(), &r.r#ref, &r.devcontainer_path)
                .await?;
            Ok(ListWorkspaceMachineTypesResponse { machine_types })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_workspace(
        &self,
        req: Request<CreateWorkspaceRequest>,
    ) -> std::result::Result<Response<Workspace>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let auth = self.read(&caller, &repo, &[]).await;
            let cx = self
                .base
                .guarded(&caller, &repo.org_id, &repo.id, "workspace.create", auth)
                .await?;
            let owner = match self.self_or_owned(&caller, &r.owner_principal_id).await {
                Ok(p) => p,
                Err(e) => {
                    self.agents
                        .forge
                        .authz
                        .audit_denial(&caller, &repo.org_id, &repo.id, "workspace.create", &e)
                        .await;
                    return Err(e);
                }
            };
            self.agents
                .workspaces
                .create(
                    &cx,
                    &repo,
                    &owner,
                    NewWorkspace {
                        name: r.name,
                        ref_name: r.r#ref,
                        devcontainer_path: r.devcontainer_path,
                        machine_type: r.machine_type,
                        idle_timeout_seconds: r.idle_timeout_seconds,
                        retention_seconds: r.retention_seconds,
                        region: r.region,
                        sparse_paths: r.sparse_paths,
                        use_prebuild: r.use_prebuild,
                        idempotency_key: r.idempotency_key,
                        agent_run_id: String::new(),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workspace(
        &self,
        req: Request<GetWorkspaceRequest>,
    ) -> std::result::Result<Response<Workspace>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async { Ok(self.ws_cx(&caller, &r.id, None).await?.1) }.await;
        reply(&caller, out)
    }

    async fn list_workspaces(
        &self,
        req: Request<ListWorkspacesRequest>,
    ) -> std::result::Result<Response<ListWorkspacesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (after, size) = page(&r.page);
            let me = caller.cx.actor_id.clone();
            let (repo_id, org_id, owner, admin) =
                if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                    let repo = self.repo(&caller, &r.repo).await?;
                    self.read(&caller, &repo, &[]).await?;
                    let admin = self
                        .agents
                        .forge
                        .authz
                        .repo(
                            &caller,
                            &repo,
                            Action::ManageSettings,
                            &[],
                            None,
                            Some(RepoRole::Maintain),
                        )
                        .await
                        .is_ok();
                    (repo.id, repo.org_id, r.owner_id.clone(), admin)
                } else if !r.org.is_empty() {
                    let org = self.org_id(&caller, &r.org).await?;
                    self.org(&caller, &org, OrgPerm::ViewMembers, None).await?;
                    let admin = self.org(&caller, &org, OrgPerm::Manage, None).await.is_ok();
                    (String::new(), org, r.owner_id.clone(), admin)
                } else {
                    let owner = if r.owner_id.is_empty() {
                        me.clone()
                    } else {
                        r.owner_id.clone()
                    };
                    self.self_or_owned(&caller, &owner).await?;
                    (String::new(), String::new(), owner, true)
                };
            // Non-admins see their own workspaces and those of agents they own.
            let owned: Vec<String> = if admin {
                Vec::new()
            } else {
                let mut v = vec![me.clone()];
                if !org_id.is_empty() {
                    for d in self.agents.definitions(&caller.cx, &org_id).await? {
                        if d.owners.contains(&me) {
                            v.push(d.agent.id.clone());
                        }
                    }
                }
                v
            };
            let (list, token) = self
                .agents
                .workspaces
                .list(
                    &caller.cx, &repo_id, &org_id, &owner, &r.states, &after, size,
                )
                .await?;
            let workspaces = list
                .into_iter()
                .map(|(w, _)| w)
                .filter(|w| admin || w.owner.as_ref().is_some_and(|o| owned.contains(&o.id)))
                .collect();
            Ok(ListWorkspacesResponse {
                workspaces,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_workspace(
        &self,
        req: Request<UpdateWorkspaceRequest>,
    ) -> std::result::Result<Response<Workspace>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self.ws_cx(&caller, &r.id, Some("workspace.update")).await?;
            self.agents
                .workspaces
                .update(
                    &cx,
                    &r.id,
                    r.display_name,
                    r.machine_type,
                    r.idle_timeout_seconds,
                    r.retention_seconds,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn start_workspace(
        &self,
        req: Request<StartWorkspaceRequest>,
    ) -> std::result::Result<Response<Workspace>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self.ws_cx(&caller, &r.id, Some("workspace.start")).await?;
            self.agents.workspaces.start(&cx, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn stop_workspace(
        &self,
        req: Request<StopWorkspaceRequest>,
    ) -> std::result::Result<Response<Workspace>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self.ws_cx(&caller, &r.id, Some("workspace.stop")).await?;
            self.agents.workspaces.stop(&cx, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_workspace(
        &self,
        req: Request<DeleteWorkspaceRequest>,
    ) -> std::result::Result<Response<DeleteWorkspaceResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self.ws_cx(&caller, &r.id, Some("workspace.delete")).await?;
            self.agents.workspaces.delete(&cx, &r.id, r.force).await?;
            Ok(DeleteWorkspaceResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn rebuild_workspace(
        &self,
        req: Request<RebuildWorkspaceRequest>,
    ) -> std::result::Result<Response<Workspace>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self
                .ws_cx(&caller, &r.id, Some("workspace.rebuild"))
                .await?;
            self.agents.workspaces.rebuild(&cx, &r.id, r.full).await
        }
        .await;
        reply(&caller, out)
    }

    async fn export_workspace_changes(
        &self,
        req: Request<ExportWorkspaceChangesRequest>,
    ) -> std::result::Result<Response<ExportWorkspaceChangesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, w, repo) = self.ws_cx(&caller, &r.id, Some("workspace.export")).await?;
            let cx = self
                .write_on(
                    &caller,
                    &repo,
                    Action::Commit,
                    Some(&w.workspace_ref),
                    "workspace.export",
                )
                .await?;
            let (changeset, r#ref, change_request) = self
                .agents
                .workspaces
                .export(
                    &cx,
                    &r.id,
                    &r.message,
                    r.provenance,
                    r.open_change_request,
                    &r.target_ref,
                )
                .await?;
            Ok(ExportWorkspaceChangesResponse {
                changeset: Some(changeset),
                r#ref: Some(r#ref),
                change_request,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workspace_connection(
        &self,
        req: Request<GetWorkspaceConnectionRequest>,
    ) -> std::result::Result<Response<WorkspaceConnection>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self
                .ws_cx(&caller, &r.id, Some("workspace.connect"))
                .await?;
            self.agents.workspaces.connection(&cx, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_workspace_prebuild_config(
        &self,
        req: Request<CreateWorkspacePrebuildConfigRequest>,
    ) -> std::result::Result<Response<WorkspacePrebuildConfig>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "workspace.prebuild_config_create",
                )
                .await?;
            self.agents
                .workspaces
                .create_prebuild_config(
                    &cx,
                    &repo,
                    WorkspacePrebuildConfig {
                        r#ref: r.r#ref,
                        devcontainer_path: r.devcontainer_path,
                        regions: r.regions,
                        machine_types: r.machine_types,
                        trigger: r.trigger,
                        schedule: r.schedule,
                        retained_versions: r.retained_versions,
                        ..Default::default()
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workspace_prebuild_config(
        &self,
        req: Request<GetWorkspacePrebuildConfigRequest>,
    ) -> std::result::Result<Response<WorkspacePrebuildConfig>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            self.agents
                .workspaces
                .get_prebuild_config(&cx, &repo, &r.id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workspace_prebuild_configs(
        &self,
        req: Request<ListWorkspacePrebuildConfigsRequest>,
    ) -> std::result::Result<Response<ListWorkspacePrebuildConfigsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (after, size) = page(&r.page);
            let (configs, token) = self
                .agents
                .workspaces
                .list_prebuild_configs(&cx, &repo, &after, size)
                .await?;
            Ok(ListWorkspacePrebuildConfigsResponse {
                configs,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_workspace_prebuild_config(
        &self,
        req: Request<UpdateWorkspacePrebuildConfigRequest>,
    ) -> std::result::Result<Response<WorkspacePrebuildConfig>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "workspace.prebuild_config_update",
                )
                .await?;
            self.agents
                .workspaces
                .update_prebuild_config(
                    &cx,
                    &repo,
                    &r.id,
                    r.regions,
                    r.machine_types,
                    r.trigger.map(|t| {
                        WorkspacePrebuildTrigger::try_from(t)
                            .unwrap_or(WorkspacePrebuildTrigger::Unspecified)
                    }),
                    r.schedule,
                    r.retained_versions,
                    r.enabled,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_workspace_prebuild_config(
        &self,
        req: Request<DeleteWorkspacePrebuildConfigRequest>,
    ) -> std::result::Result<Response<DeleteWorkspacePrebuildConfigResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "workspace.prebuild_config_delete",
                )
                .await?;
            self.agents
                .workspaces
                .delete_prebuild_config(&cx, &repo, &r.id)
                .await?;
            Ok(DeleteWorkspacePrebuildConfigResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn trigger_workspace_prebuild(
        &self,
        req: Request<TriggerWorkspacePrebuildRequest>,
    ) -> std::result::Result<Response<WorkspacePrebuild>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .write(
                    &caller,
                    &repo,
                    Action::ManageSettings,
                    "workspace.prebuild_trigger",
                )
                .await?;
            self.agents
                .workspaces
                .trigger_prebuild(&cx, &repo, &r.config_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workspace_prebuilds(
        &self,
        req: Request<ListWorkspacePrebuildsRequest>,
    ) -> std::result::Result<Response<ListWorkspacePrebuildsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.read(&caller, &repo, &[]).await?;
            let (after, size) = page(&r.page);
            let (prebuilds, token) = self
                .agents
                .workspaces
                .list_prebuilds(&cx, &repo, &r.config_id, r.state, &after, size)
                .await?;
            Ok(ListWorkspacePrebuildsResponse {
                prebuilds,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_workspace_ports(
        &self,
        req: Request<ListWorkspacePortsRequest>,
    ) -> std::result::Result<Response<ListWorkspacePortsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self.ws_cx(&caller, &r.workspace_id, None).await?;
            Ok(ListWorkspacePortsResponse {
                ports: self.agents.workspaces.ports(&cx, &r.workspace_id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_workspace_port_visibility(
        &self,
        req: Request<SetWorkspacePortVisibilityRequest>,
    ) -> std::result::Result<Response<WorkspacePort>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, _, _) = self
                .ws_cx(&caller, &r.workspace_id, Some("workspace.port_visibility"))
                .await?;
            self.agents
                .workspaces
                .set_port_visibility(
                    &cx,
                    &r.workspace_id,
                    r.port,
                    WorkspacePortVisibility::try_from(r.visibility)
                        .unwrap_or(WorkspacePortVisibility::Unspecified),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workspace_dotfiles(
        &self,
        req: Request<GetWorkspaceDotfilesRequest>,
    ) -> std::result::Result<Response<WorkspaceDotfiles>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let p = self.self_or_owned(&caller, &r.principal_id).await?;
            self.agents.workspaces.dotfiles(&caller.cx, &p.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_workspace_dotfiles(
        &self,
        req: Request<SetWorkspaceDotfilesRequest>,
    ) -> std::result::Result<Response<WorkspaceDotfiles>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let p = self.self_or_owned(&caller, &r.principal_id).await?;
            // The dotfiles repo must be readable by the principal setting it.
            let mut repo_ref = None;
            let mut org = String::new();
            if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                let repo = self.repo(&caller, &r.repo).await?;
                self.read(&caller, &repo, &[]).await?;
                org = repo.org_id.clone();
                repo_ref = super::registry::repo_ref_id(&repo.id);
            }
            self.agents
                .workspaces
                .set_dotfiles(
                    &caller.cx,
                    &org,
                    WorkspaceDotfiles {
                        principal_id: p.id,
                        repo: repo_ref,
                        install_command: r.install_command,
                        enabled: r.enabled,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workspace_egress_policy(
        &self,
        req: Request<GetWorkspaceEgressPolicyRequest>,
    ) -> std::result::Result<Response<WorkspaceEgressPolicy>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, scope, org, repo) = self.egress_target(&caller, &r.scope, false).await?;
            self.agents
                .workspaces
                .egress_policy(&cx, &scope, &org, &repo)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_workspace_egress_policy(
        &self,
        req: Request<SetWorkspaceEgressPolicyRequest>,
    ) -> std::result::Result<Response<WorkspaceEgressPolicy>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (cx, scope, org, repo) = self.egress_target(&caller, &r.scope, true).await?;
            self.agents
                .workspaces
                .set_egress_policy(
                    &cx,
                    &org,
                    &repo,
                    WorkspaceEgressPolicy {
                        scope: Some(scope),
                        default_action: r.default_action,
                        allowed_domains: r.allowed_domains,
                        allowed_cidrs: r.allowed_cidrs,
                        denied_domains: r.denied_domains,
                        allow_package_registries: r.allow_package_registries,
                        log_denied: r.log_denied,
                        ..Default::default()
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_workspace_usage(
        &self,
        req: Request<GetWorkspaceUsageRequest>,
    ) -> std::result::Result<Response<WorkspaceUsage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.org_id(&caller, &r.org).await?;
            let cx = if !r.principal_id.is_empty() && r.principal_id == caller.cx.actor_id {
                self.org(&caller, &org, OrgPerm::ViewMembers, None).await?
            } else {
                self.org(&caller, &org, OrgPerm::Manage, None).await?
            };
            let end = match ms(&r.window_end) {
                0 => u64::MAX,
                e => e,
            };
            self.agents
                .workspaces
                .usage(&cx, &org, &r.principal_id, ms(&r.window_start), end)
                .await
        }
        .await;
        reply(&caller, out)
    }
}

/// Schedules of a request.
fn schedules_from_wire(v: Vec<AgentSchedule>) -> Vec<Schedule> {
    v.into_iter()
        .map(|s| Schedule {
            id: s.id,
            repo_id: s.repo_id,
            cron: s.cron,
            prompt: s.prompt,
            last_fired_ms: 0,
        })
        .collect()
}

/// A definition as the wire message.
fn wire_definition(d: AgentDefinition) -> bee_proto::v1::AgentDefinition {
    bee_proto::v1::AgentDefinition {
        org: d.org_id,
        agent: Some(d.agent),
        name: d.name,
        runtime_id: d.runtime_id,
        runtime: d.runtime,
        model: d.model,
        instructions_path: d.instructions_path,
        toolsets: d.toolsets,
        default_budget: Some(d.default_budget),
        owners: d.owners,
        purpose: d.purpose,
        enabled: d.enabled,
        schedules: d
            .schedules
            .into_iter()
            .map(|s| AgentSchedule {
                id: s.id,
                repo_id: s.repo_id,
                cron: s.cron,
                prompt: s.prompt,
                last_fired_at: (s.last_fired_ms > 0).then(|| crate::ids::ts(s.last_fired_ms)),
            })
            .collect(),
        created_at: Some(crate::ids::ts(d.created_ms)),
        updated_at: Some(crate::ids::ts(d.updated_ms)),
    }
}
