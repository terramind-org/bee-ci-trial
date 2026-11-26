//! Repository authorization and state transitions for SwarmService.

use std::sync::Arc;

use bee_auth::Request as ScopeRequest;
use bee_policy::Action;
use bee_proto::v1::swarm_service_server::{SwarmService, SwarmServiceServer};
use bee_proto::v1::*;
use serde_json::json;
use tonic::{Request, Response, Status};

use super::model::{self, Applicant, Record, invalid_state};
use super::store;
use crate::agents::registry::repo_ref_id;
use crate::agents::runs::{StartRun, Trigger, terminal};
use crate::agents::{Agents, SURFACE};
use crate::auth::{Caller, authorize_repo_scope};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::service::{ForgeServices, next, page, reply};
use crate::store::{Row, Tx};

mod joins;
pub(super) mod tasks;

/// The SwarmService over the forge's agent runs.
#[derive(Clone)]
pub struct SwarmApi {
    agents: Arc<Agents>,
    base: ForgeServices,
}

fn deny(message: &str) -> ForgeError {
    ForgeError::permission_denied(message).with_subcode("swarm_permission")
}

fn check_scope(
    caller: &Caller,
    repo: &Repo,
    action: Action,
    branch: &str,
    now_ms: u64,
) -> Result<()> {
    authorize_repo_scope(
        &caller.token,
        repo,
        |repo| ScopeRequest::new(repo, action).on_ref(branch),
        now_ms / 1000,
    )
    .map_err(ForgeError::from)
}

fn public_principal(mut principal: Principal) -> Principal {
    principal.external_ids.clear();
    principal
}

fn is_owner(caller: &Caller, swarm: &Swarm) -> bool {
    caller.cx.actor_kind == PrincipalKind::Human
        && swarm
            .owner
            .as_ref()
            .is_some_and(|p| p.id == caller.cx.actor_id)
}

fn is_run(caller: &Caller, agent: Option<&Principal>, token: &str) -> bool {
    caller.cx.actor_kind == PrincipalKind::Agent
        && agent.is_some_and(|p| p.id == caller.cx.actor_id)
        && caller.token.block_ids().iter().any(|id| id.0 == token)
}

fn is_lead(caller: &Caller, record: &Record) -> bool {
    is_run(caller, record.swarm.lead.as_ref(), &record.lead_token_id)
}

fn lead_or_owner(caller: &Caller, record: &Record) -> Result<()> {
    if is_lead(caller, record) || is_owner(caller, &record.swarm) {
        Ok(())
    } else {
        Err(deny("The swarm lead or its owner must make this change"))
    }
}

// Retain the originating reason and payload but always identify swarm RPC errors.
fn swarm_reply<T>(caller: &Caller, result: Result<T>) -> std::result::Result<Response<T>, Status> {
    reply(
        caller,
        result.map_err(|e| {
            if e.subcode.is_empty() {
                e.with_subcode("swarm_request")
            } else {
                e
            }
        }),
    )
}

impl SwarmApi {
    /// Constructs a service using the agent dispatcher, costs and messages.
    pub fn new(agents: Arc<Agents>) -> Self {
        Self {
            base: ForgeServices::new(agents.forge.clone()),
            agents,
        }
    }

    /// The shared gRPC implementation, also used by Connect.
    pub fn server(&self) -> SwarmServiceServer<Self> {
        SwarmServiceServer::new(self.clone())
    }

    async fn access(
        &self,
        caller: &Caller,
        repo: &Repo,
        action: Action,
        branch: Option<&str>,
        role: Option<RepoRole>,
    ) -> Result<Cx> {
        self.agents
            .forge
            .authz
            .repo(caller, repo, action, &[], branch, role)
            .await
    }

    async fn audited(
        &self,
        caller: &Caller,
        repo: &Repo,
        event: &str,
        auth: Result<Cx>,
    ) -> Result<Cx> {
        self.base
            .guarded(caller, &repo.org_id, &repo.id, event, auth)
            .await
    }

    fn writable(repo: &Repo) -> Result<()> {
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name).with_subcode("swarm_archived"));
        }
        Ok(())
    }

    async fn resolve(
        &self,
        caller: &Caller,
        target: &Option<SwarmRef>,
    ) -> Result<(Repo, String, Cx)> {
        let target = target
            .as_ref()
            .ok_or_else(|| ForgeError::missing("swarm"))?;
        let repo = self.base.repo(caller, &target.repo).await?;
        let cx = self.access(caller, &repo, Action::Read, None, None).await?;
        Ok((repo, target.id.clone(), cx))
    }

    async fn view(&self, caller: &Caller, repo: &Repo, cx: &Cx, mut swarm: Swarm) -> Result<Swarm> {
        swarm = store::hydrate(&self.agents, cx, repo, swarm).await?;
        let admin = self.may_view_cost(caller, repo).await;
        self.redact(caller, &mut swarm, admin).await;
        Ok(swarm)
    }

    async fn may_view_cost(&self, caller: &Caller, repo: &Repo) -> bool {
        self.access(caller, repo, Action::Read, None, Some(RepoRole::Admin))
            .await
            .is_ok()
    }

    async fn redact(&self, caller: &Caller, swarm: &mut Swarm, admin: bool) {
        let is_lead = caller.cx.actor_kind == PrincipalKind::Agent
            && swarm
                .lead
                .as_ref()
                .is_some_and(|p| p.id == caller.cx.actor_id)
            && self
                .agents
                .run_rec(&caller.cx, &swarm.lead_run_id)
                .await
                .is_ok_and(|r| is_run(caller, swarm.lead.as_ref(), &r.token_id));
        if !is_lead && !is_owner(caller, swarm) {
            if !admin {
                swarm.budget = None;
                swarm.cost = None;
            }
            swarm.joins.retain(|j| {
                j.requester
                    .as_ref()
                    .is_some_and(|p| p.id == caller.cx.actor_id)
            });
        }
    }

    async fn create(&self, caller: &Caller, r: CreateSwarmRequest) -> Result<Swarm> {
        let repo = self.base.repo(caller, &r.repo).await?;
        let cx = self
            .audited(
                caller,
                &repo,
                "swarm.created",
                self.access(caller, &repo, Action::Commit, Some(&r.branch), None)
                    .await,
            )
            .await?;
        Self::writable(&repo)?;
        model::line("name", &r.name, 200)?;
        model::line("branch", &r.branch, 1024)?;
        if r.goal.trim().is_empty() {
            return Err(ForgeError::missing("goal"));
        }
        crate::agents::check_len("goal", &r.goal, 16_384)?;
        bee_core::RefName::new(&r.branch)
            .map_err(|_| ForgeError::invalid("swarm_branch", "Expected a branch name"))?;
        self.agents
            .changes
            .vcs()
            .ref_tip(&repo, &r.branch)
            .await?
            .ok_or_else(|| ForgeError::not_found("Swarm branch").with_subcode("swarm_branch"))?;
        if !r.issue.is_empty() {
            let number = r
                .issue
                .parse()
                .map_err(|_| ForgeError::invalid("swarm_issue", "Expected an issue number"))?;
            let issue = self
                .agents
                .forge
                .work
                .get_issue_by_number(&cx, &(&repo).into(), number)
                .await?;
            if issue.repo_id != repo.id {
                return Err(
                    ForgeError::not_found("Issue in this repository").with_subcode("swarm_issue")
                );
            }
        }
        let lead = store::run(
            &mut self.agents.db.begin(SURFACE, &cx).await?,
            &repo.id,
            &r.lead_run_id,
            false,
        )
        .await?;
        let owner = self
            .agents
            .forge
            .hooks
            .directory
            .get(&lead.owner_id)
            .await
            .filter(|p| p.kind == PrincipalKind::Human as i32)
            .ok_or_else(|| {
                ForgeError::invalid("swarm_owner", "The lead run needs a human owner")
            })?;
        if caller.cx.actor_kind != PrincipalKind::Human || caller.cx.actor_id != owner.id {
            return Err(self
                .audited(
                    caller,
                    &repo,
                    "swarm.created",
                    Err(deny("The lead run's owner must create the swarm")),
                )
                .await
                .unwrap_err());
        }
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        // A swarm cannot enter planning while its organization has stopped agent work.
        crate::safety::control::require_running_in_tx(&mut tx, &repo.org_id).await?;
        let run = store::run(&mut tx, &repo.id, &r.lead_run_id, true)
            .await?
            .run();
        if terminal(run.state) || run.base_ref != r.branch {
            return Err(invalid_state(
                "The lead must be an active run on the swarm branch",
            ));
        }
        let now = ts(tx.now_ms());
        let record = Record {
            swarm: Swarm {
                id: tx.new_id()?,
                repo: repo_ref_id(&repo.id),
                name: r.name,
                goal: r.goal,
                issue: r.issue,
                branch: r.branch,
                lead_run_id: run.id.clone(),
                lead: run.agent.map(public_principal),
                owner: Some(public_principal(owner)),
                state: SwarmState::Planning as i32,
                member_run_ids: vec![run.id],
                created_at: Some(now),
                updated_at: Some(now),
                ..Default::default()
            },
            applicants: Vec::new(),
            lead_token_id: lead.token_id,
        };
        store::insert(&mut tx, &repo, &record).await?;
        tx.commit().await?;
        self.view(caller, &repo, &cx, record.swarm).await
    }

    async fn mutate(
        &self,
        caller: &Caller,
        target: &Option<SwarmRef>,
    ) -> Result<(Repo, Cx, Tx, Record, Row)> {
        let (repo, id, cx) = self.resolve(caller, target).await?;
        Self::writable(&repo)?;
        let cx = if caller.cx.actor_kind == PrincipalKind::Human {
            self.access(caller, &repo, Action::Commit, None, None)
                .await?
        } else {
            cx
        };
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let (r, row) = store::load(&mut tx, &repo.id, &id, true).await?;
        check_scope(caller, &repo, Action::Commit, &r.swarm.branch, tx.now_ms())?;
        Ok((repo, cx, tx, r, row))
    }

    async fn set_state(&self, caller: &Caller, r: SetSwarmStateRequest) -> Result<Swarm> {
        let (repo, id, cx) = self.resolve(caller, &r.swarm).await?;
        Self::writable(&repo)?;
        let (snapshot, _) = store::load(
            &mut self.agents.db.begin(SURFACE, &cx).await?,
            &repo.id,
            &id,
            false,
        )
        .await?;
        let role = (!is_owner(caller, &snapshot.swarm)).then_some(RepoRole::Admin);
        let action = if role.is_some() {
            Action::ManageSettings
        } else {
            Action::Commit
        };
        let cx = self
            .audited(
                caller,
                &repo,
                "swarm.state_changed",
                self.access(caller, &repo, action, Some(&snapshot.swarm.branch), role)
                    .await,
            )
            .await?;
        let state = r.state();
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        if state == SwarmState::Working {
            // Share the stop lock before taking the swarm lock, as run admission does.
            crate::safety::control::require_running_in_tx(&mut tx, &repo.org_id).await?;
        }
        let (mut record, row) = store::load(&mut tx, &repo.id, &id, true).await?;
        if record.swarm.state() == state {
            drop(tx);
            return self.view(caller, &repo, &cx, record.swarm).await;
        }
        model::transition(record.swarm.state(), state)?;
        if state == SwarmState::Working
            && record.swarm.state() == SwarmState::Paused
            && let Some(owner) = &record.swarm.owner
            && let Some(attention) =
                crate::safety::unacknowledged_swarm_pause(&mut tx, &repo.id, &id, &owner.id).await?
        {
            return Err(ForgeError::invalid_state(format!(
                "Acknowledge safety attention item {attention} before resuming this swarm"
            ))
            .with_subcode(&format!(
                "swarm_attention_open_{}",
                attention.to_ascii_lowercase()
            )));
        }
        let mut cancelled = Vec::new();
        if state == SwarmState::Stopped {
            for run in &record.swarm.member_run_ids {
                if let Some(c) = self
                    .agents
                    .cancel_in_tx(&cx, &mut tx, run, "Swarm stopped")
                    .await?
                {
                    cancelled.push(c);
                }
            }
            tasks::interrupt_attempts(&mut tx, &repo, &mut record, None, "Swarm stopped").await?;
        } else if state == SwarmState::Finished {
            if record
                .swarm
                .tasks
                .iter()
                .any(|t| matches!(t.state(), SwarmTaskState::Open | SwarmTaskState::Working))
            {
                return Err(invalid_state(
                    "Every task must end before the swarm finishes",
                ));
            }
            for id in &record.swarm.member_run_ids {
                if !terminal(store::run(&mut tx, &repo.id, id, true).await?.run().state) {
                    return Err(invalid_state(
                        "Every member run must end before the swarm finishes",
                    ));
                }
            }
        }
        if matches!(state, SwarmState::Stopped | SwarmState::Finished) {
            joins::close_pending(&mut tx, &repo, &mut record).await?;
        }
        record.swarm.state = state as i32;
        store::save(
            &mut tx,
            &repo,
            &mut record,
            &row,
            "swarm.state_changed",
            json!({"state": state.as_str_name()}),
        )
        .await?;
        tx.commit().await?;
        for (run, was_running) in cancelled {
            self.agents.after_cancel(&run, was_running).await;
        }
        if state == SwarmState::Working {
            self.agents.wake.notify_one();
        }
        self.view(caller, &repo, &cx, record.swarm).await
    }

    async fn set_status(&self, caller: &Caller, r: SetSwarmStatusRequest) -> Result<Swarm> {
        model::line("status", &r.status, 500)?;
        let (repo, cx, mut tx, mut record, row) = self.mutate(caller, &r.swarm).await?;
        store::require_active(&record.swarm)?;
        if !is_lead(caller, &record) {
            return Err(deny("Only the lead run may write the swarm status"));
        }
        record.swarm.status = r.status;
        store::save(
            &mut tx,
            &repo,
            &mut record,
            &row,
            "swarm.status_changed",
            json!({}),
        )
        .await?;
        tx.commit().await?;
        self.view(caller, &repo, &cx, record.swarm).await
    }

    async fn message(&self, caller: &Caller, r: SendSwarmMessageRequest) -> Result<AgentRunEvent> {
        let (repo, id, cx) = self.resolve(caller, &r.swarm).await?;
        Self::writable(&repo)?;
        let (record, _) = store::load(
            &mut self.agents.db.begin(SURFACE, &cx).await?,
            &repo.id,
            &id,
            false,
        )
        .await?;
        let auth = if caller.cx.actor_kind == PrincipalKind::Human && !caller.is_anonymous() {
            self.access(
                caller,
                &repo,
                Action::Commit,
                Some(&record.swarm.branch),
                None,
            )
            .await
        } else {
            Err(deny("Only a person may send a swarm message"))
        };
        let cx = self
            .audited(caller, &repo, "agent.run_message", auth)
            .await?;
        store::require_active(&record.swarm)?;
        self.agents
            .send_message(&cx, &record.swarm.lead_run_id, &r.body)
            .await
    }
}

#[tonic::async_trait]
impl SwarmService for SwarmApi {
    async fn create_swarm(
        &self,
        req: Request<CreateSwarmRequest>,
    ) -> std::result::Result<Response<Swarm>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.create(&caller, req.into_inner()).await)
    }

    async fn get_swarm(
        &self,
        req: Request<SwarmRef>,
    ) -> std::result::Result<Response<Swarm>, Status> {
        let caller = self.base.caller(&req)?;
        let out = async {
            let (repo, id, cx) = self.resolve(&caller, &Some(req.into_inner())).await?;
            let (record, _) = store::load(
                &mut self.agents.db.begin(SURFACE, &cx).await?,
                &repo.id,
                &id,
                false,
            )
            .await?;
            self.view(&caller, &repo, &cx, record.swarm).await
        }
        .await;
        swarm_reply(&caller, out)
    }

    async fn list_repo_work(
        &self,
        req: Request<ListRepoWorkRequest>,
    ) -> std::result::Result<Response<ListRepoWorkResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let out = async {
            let r = req.into_inner();
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .access(&caller, &repo, Action::Read, None, None)
                .await?;
            let (after, size) = page(&r.page);
            let (mut items, token) = store::list(&self.agents, &cx, &repo, &after, size).await?;
            let admin = self.may_view_cost(&caller, &repo).await;
            for item in &mut items {
                match &mut item.work {
                    Some(repo_work_item::Work::Swarm(s)) => {
                        self.redact(&caller, s, admin).await;
                    }
                    Some(repo_work_item::Work::Run(run)) => {
                        let owner = run
                            .agent
                            .as_ref()
                            .is_some_and(|p| p.owner_id == cx.actor_id);
                        let starter = run.started_by.as_ref().is_some_and(|p| p.id == cx.actor_id);
                        if !admin
                            && !owner
                            && !starter
                            && !run.agent.as_ref().is_some_and(|p| p.id == cx.actor_id)
                        {
                            run.budget = None;
                            run.cost = None;
                        }
                        for principal in [&mut run.agent, &mut run.started_by].into_iter().flatten()
                        {
                            principal.external_ids.clear();
                        }
                    }
                    None => {}
                }
            }
            Ok(ListRepoWorkResponse {
                items,
                page: next(token),
            })
        }
        .await;
        swarm_reply(&caller, out)
    }

    async fn set_swarm_state(
        &self,
        req: Request<SetSwarmStateRequest>,
    ) -> std::result::Result<Response<Swarm>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.set_state(&caller, req.into_inner()).await)
    }

    async fn set_swarm_status(
        &self,
        req: Request<SetSwarmStatusRequest>,
    ) -> std::result::Result<Response<Swarm>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.set_status(&caller, req.into_inner()).await)
    }

    async fn add_swarm_task(
        &self,
        req: Request<AddSwarmTaskRequest>,
    ) -> std::result::Result<Response<SwarmTask>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.add_task(&caller, req.into_inner()).await)
    }

    async fn start_swarm_task(
        &self,
        req: Request<StartSwarmTaskRequest>,
    ) -> std::result::Result<Response<SwarmTask>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.start_task(&caller, req.into_inner()).await)
    }

    async fn finish_swarm_task(
        &self,
        req: Request<FinishSwarmTaskRequest>,
    ) -> std::result::Result<Response<SwarmTask>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.finish_task(&caller, req.into_inner()).await)
    }

    async fn send_swarm_message(
        &self,
        req: Request<SendSwarmMessageRequest>,
    ) -> std::result::Result<Response<AgentRunEvent>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.message(&caller, req.into_inner()).await)
    }

    async fn request_swarm_join(
        &self,
        req: Request<RequestSwarmJoinRequest>,
    ) -> std::result::Result<Response<SwarmJoin>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.request_join(&caller, req.into_inner()).await)
    }

    async fn decide_swarm_join(
        &self,
        req: Request<DecideSwarmJoinRequest>,
    ) -> std::result::Result<Response<SwarmJoin>, Status> {
        let caller = self.base.caller(&req)?;
        swarm_reply(&caller, self.decide_join(&caller, req.into_inner()).await)
    }
}
