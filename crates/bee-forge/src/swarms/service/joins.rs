//! Admission rechecks current repository policy and ownership. No bearer credentials
//! from applicants are saved or handed to the lead.

use super::*;
use crate::orgs::role_name;
use bee_policy::{Actor, PrincipalKind as PolicyKind};

const MAX_JOINS: usize = 200;

fn kind(principal: &Principal) -> PolicyKind {
    match principal.kind() {
        PrincipalKind::Agent => PolicyKind::Agent,
        PrincipalKind::Service => PolicyKind::Service,
        _ => PolicyKind::Human,
    }
}

impl SwarmApi {
    pub(super) async fn request_join(
        &self,
        caller: &Caller,
        req: RequestSwarmJoinRequest,
    ) -> Result<SwarmJoin> {
        let budget = req.budget.filter(|b| b.max_usd_micros > 0).ok_or_else(|| {
            ForgeError::invalid("swarm_join_budget", "A positive spending limit is required")
        })?;
        let (repo, id, cx) = self.resolve(caller, &req.swarm).await?;
        Self::writable(&repo)?;
        if caller.is_anonymous() {
            return Err(deny("Sign in to request membership"));
        }
        let agent = self
            .agents
            .forge
            .hooks
            .directory
            .get(&req.agent_principal_id)
            .await
            .filter(|p| p.kind() == PrincipalKind::Agent)
            .ok_or_else(|| {
                ForgeError::not_found("Agent principal").with_subcode("swarm_join_agent")
            })?;
        if cx.actor_id != agent.owner_id && cx.actor_id != agent.id {
            return Err(deny("Offer an agent that you own"));
        }
        let requester = self
            .agents
            .forge
            .hooks
            .directory
            .get(&cx.actor_id)
            .await
            .unwrap_or_else(|| cx.principal());
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let (mut record, row) = store::load(&mut tx, &repo.id, &id, true).await?;
        store::require_active(&record.swarm)?;
        check_scope(
            caller,
            &repo,
            Action::SwarmJoin,
            &record.swarm.branch,
            tx.now_ms(),
        )?;
        if record.swarm.joins.len() >= MAX_JOINS {
            return Err(ForgeError::too_large(
                "joins",
                MAX_JOINS as u64,
                (MAX_JOINS + 1) as u64,
            ));
        }
        if record.swarm.joins.iter().any(|j| {
            j.agent.as_ref().is_some_and(|p| p.id == agent.id)
                && matches!(
                    j.state(),
                    SwarmJoinState::Pending | SwarmJoinState::Accepted
                )
        }) {
            return Err(ForgeError::already_exists(
                "This agent already has a pending or accepted request",
            )
            .with_subcode("swarm_join_duplicate"));
        }
        let join = SwarmJoin {
            id: tx.new_id()?,
            requester: Some(public_principal(requester)),
            agent: Some(public_principal(agent)),
            budget: Some(budget),
            state: SwarmJoinState::Pending as i32,
            requested_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        record.applicants.push(Applicant {
            join_id: join.id.clone(),
            chain: cx.chain.clone(),
        });
        record.swarm.joins.push(join.clone());
        store::save(
            &mut tx,
            &repo,
            &mut record,
            &row,
            "swarm.join_requested",
            json!({"join_id": join.id}),
        )
        .await?;
        tx.commit().await?;
        Ok(join)
    }

    pub(super) async fn decide_join(
        &self,
        caller: &Caller,
        req: DecideSwarmJoinRequest,
    ) -> Result<SwarmJoin> {
        if !req.reason.is_empty() {
            model::line("reason", &req.reason, 500)?;
        }
        let (repo, id, cx) = self.resolve(caller, &req.swarm).await?;
        Self::writable(&repo)?;
        let (mut record, _) = store::load(
            &mut self.agents.db.begin(SURFACE, &cx).await?,
            &repo.id,
            &id,
            false,
        )
        .await?;
        let auth = match lead_or_owner(caller, &record) {
            Ok(()) => {
                self.access(
                    caller,
                    &repo,
                    Action::Commit,
                    Some(&record.swarm.branch),
                    None,
                )
                .await
            }
            Err(e) => Err(e),
        };
        let cx = self
            .audited(caller, &repo, "swarm.join_decided", auth)
            .await?;
        store::require_active(&record.swarm)?;
        let join = store::pending_join(&mut record, &req.join_id)?.clone();
        if !req.accept {
            return self
                .write_decision(
                    &cx,
                    &repo,
                    &id,
                    &join.id,
                    SwarmJoinState::Declined,
                    req.reason,
                )
                .await;
        }
        let agent = join
            .agent
            .as_ref()
            .ok_or_else(|| ForgeError::internal("Join has no agent"))?;
        let applicant = record
            .applicants
            .iter()
            .find(|a| a.join_id == join.id)
            .ok_or_else(|| ForgeError::internal("Join has no applicant"))?;
        let def = self.agents.definition(&cx, &repo.org_id, &agent.id).await?;
        if !applicant.chain.iter().any(|p| p == def.owner()) || def.agent.owner_id != agent.owner_id
        {
            return Err(deny(
                "The agent's current primary owner must authorize this run",
            ));
        }
        if let Err(e) = self
            .join_policy(caller, &cx, &repo, &def.agent, &applicant.chain)
            .await
        {
            if e.reason != ErrorReason::PolicyDenied {
                return Err(e);
            }
            return self
                .write_decision(
                    &cx,
                    &repo,
                    &id,
                    &join.id,
                    SwarmJoinState::Refused,
                    e.message,
                )
                .await;
        }
        let mut chain = vec![def.owner().to_owned()];
        chain.extend(
            applicant
                .chain
                .iter()
                .filter(|id| *id != def.owner())
                .cloned(),
        );
        self.agents
            .start_run(
                &cx,
                StartRun {
                    repo: repo.clone(),
                    task: AgentTask {
                        kind: AgentTaskKind::Freeform as i32,
                        prompt: format!(
                            "Join swarm {} in Beehive. Goal: {}\nCoordinate tasks with the lead.",
                            record.swarm.id, record.swarm.goal,
                        ),
                        ..Default::default()
                    },
                    agent_id: agent.id.clone(),
                    base_ref: record.swarm.branch.clone(),
                    scope: AgentRunScope {
                        actions: vec!["read".into(), "commit".into()],
                        ..Default::default()
                    },
                    budget: join.budget,
                    chain,
                    trigger: Trigger::SwarmJoin,
                    trigger_ref: format!("{id}/{}", join.id),
                    swarm_join_reason: req.reason,
                    ..Default::default()
                },
            )
            .await?;
        let (record, _) = store::load(
            &mut self.agents.db.begin(SURFACE, &cx).await?,
            &repo.id,
            &id,
            false,
        )
        .await?;
        record
            .swarm
            .joins
            .into_iter()
            .find(|j| j.id == join.id)
            .ok_or_else(|| ForgeError::internal("Accepted join is missing"))
    }

    async fn join_policy(
        &self,
        caller: &Caller,
        cx: &Cx,
        repo: &Repo,
        agent: &Principal,
        chain: &[String],
    ) -> Result<()> {
        let Some(source) = &self.agents.forge.hooks.policy else {
            return Ok(());
        };
        let Some(policy) = source.policy(repo).await? else {
            return Ok(());
        };
        let mut actors = self.agents.forge.authz.policy_actors(caller, repo).await?;
        let directory = &self.agents.forge.hooks.directory;
        for id in std::iter::once(&agent.id).chain(chain.iter()) {
            if actors.iter().any(|a| a.id.as_str() == id) {
                continue;
            }
            let p = directory
                .get(id)
                .await
                .ok_or_else(|| deny("An applicant principal is missing"))?;
            let perms = self.agents.forge.orgs.effective_role(cx, repo, id).await?;
            let actor = Actor::new(id, kind(&p)).with_repo_role(role_name(perms.role()));
            actors.push(actor);
        }
        let decision = policy.check_access_chain(&actors, Action::SwarmJoin, "");
        if !decision.allowed {
            return Err(ForgeError::policy_denied(
                &decision.rule,
                vec!["swarm_join".into()],
                decision.detail,
            )
            .with_subcode("swarm_join_policy"));
        }
        Ok(())
    }

    async fn write_decision(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        join_id: &str,
        state: SwarmJoinState,
        reason: String,
    ) -> Result<SwarmJoin> {
        let mut tx = self.agents.db.begin(SURFACE, cx).await?;
        let (mut record, row) = store::load(&mut tx, &repo.id, id, true).await?;
        store::require_active(&record.swarm)?;
        let join = store::pending_join(&mut record, join_id)?;
        join.state = state as i32;
        join.reason = reason;
        join.decided_at = Some(ts(tx.now_ms()));
        let join = join.clone();
        store::save(
            &mut tx,
            repo,
            &mut record,
            &row,
            "swarm.join_decided",
            json!({"join_id": join_id, "state": state.as_str_name()}),
        )
        .await?;
        tx.commit().await?;
        Ok(join)
    }
}

pub(super) async fn close_pending(tx: &mut Tx, repo: &Repo, record: &mut Record) -> Result<()> {
    let mut events = Vec::new();
    let now = ts(tx.now_ms());
    for join in &mut record.swarm.joins {
        if join.state() == SwarmJoinState::Pending {
            join.state = SwarmJoinState::Declined as i32;
            join.reason = "Swarm ended".into();
            join.decided_at = Some(now);
            events.push(json!({"join_id": join.id, "state": "SWARM_JOIN_STATE_DECLINED"}));
        }
    }
    for event in events {
        store::emit(
            tx,
            &repo.id,
            &repo.org_id,
            &record.swarm,
            "swarm.join_decided",
            event,
        )
        .await?;
    }
    Ok(())
}
