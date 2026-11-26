//! Agent runs: start (with a delegated, attenuated token whose chain ends at a human or
//! service root), the dispatch queue, execution through the [`AgentRunner`], session events,
//! cost and budgets, cancel, retry, steering, and provenance sessions queryable by change.
//!
//! # Lifecycle
//!
//! `QUEUED` → `PROVISIONING` (claimed by the dispatcher; a workspace is created when the run
//! asked for a machine type) → `RUNNING` (the runner executes) → `SUCCEEDED`, `FAILED`,
//! `CANCELLED`, or `WAITING_FOR_REVIEW` when the run opened change requests that are still
//! open. A waiting run becomes `SUCCEEDED` when its change requests land and `FAILED`
//! (`change_closed`) when they are closed unlanded. Every transition is a session event
//! (`STATE_CHANGED`), a forge event and an audit entry.
//!
//! # Tokens
//!
//! Runs started through the API derive their token from the caller's own token (attenuation
//! with `act_as` = the agent), so the run can never do more than the caller. Runs started by
//! events (assignment, mentions, watchers, schedules, review configuration) get a token minted
//! for the agent on behalf of its owner and the human who caused the event. Either way the
//! token is limited to the repo, the definition's toolsets, the run's refs and paths, the
//! budget's changeset and land counts, carries the run's session id, and is revoked when the
//! run ends.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use bee_auth::{Budget, PrincipalRef, Scope};
use bee_proto::v1::{
    AgentRun, AgentRunBudget, AgentRunEvent, AgentRunEventKind as Ek, AgentRunScope,
    AgentRunState as St, AgentTask, AgentTaskKind, Cost, Event, IssueLinkSource, ObjectId,
    Principal, PrincipalKind, Repo, SubjectKind, SubjectRef,
};
use tokio::task::JoinSet;

use super::model::{RunRec, RunTokenRec, SessionRec};
use super::ports::{RunGrant, RunJob, RunOutcome, RunReporter, RunStatus, RunnerEvent};
use super::registry::{
    AgentDefinition, add_cost, charge, ensure_budget, insert_locator, locate, put_locator,
    repo_ref_id, run_repo,
};
use super::*;
use crate::audit::AuditDraft;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Row, Scan, Tx};
use crate::work::{ChangeLink, RepoInfo};

/// Most events kept per run.
pub const MAX_RUN_EVENTS: u64 = 100_000;
/// Longest steering message.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// What started a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Trigger {
    /// `StartAgentRun`.
    #[default]
    Manual,
    /// The agent was assigned an issue.
    IssueAssignment,
    /// The agent was @mentioned in an issue or CR comment.
    Mention,
    /// A component watcher saw changes in its paths.
    Watcher,
    /// Automatic agent review.
    ReviewConfig,
    /// A schedule of the agent definition.
    Schedule,
    /// Changes were requested on a CR the agent opened.
    Revise,
    /// `RetryAgentRun`.
    Retry,
    /// A post published in a repo the agent follows (its feed trigger).
    Feed,
    /// A worker admitted to a swarm.
    SwarmJoin,
}

impl Trigger {
    /// Stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Manual => "manual",
            Trigger::IssueAssignment => "issue_assignment",
            Trigger::Mention => "mention",
            Trigger::Watcher => "watcher",
            Trigger::ReviewConfig => "review_config",
            Trigger::Schedule => "schedule",
            Trigger::Revise => "revise",
            Trigger::Retry => "retry",
            Trigger::Feed => "feed",
            Trigger::SwarmJoin => "swarm_join",
        }
    }
}

/// A run to start.
#[derive(Clone, Debug, Default)]
pub struct StartRun {
    /// The repo.
    pub repo: Repo,
    /// The task.
    pub task: AgentTask,
    /// Agent principal (empty = the starter's default agent in the org).
    pub agent_id: String,
    /// Runtime id (empty = the definition's).
    pub runtime_id: String,
    /// Model (empty = the definition's).
    pub model: String,
    /// Base ref (empty = the default ref).
    pub base_ref: String,
    /// Requested scope (never broader than the definition's toolsets).
    pub scope: AgentRunScope,
    /// Requested budget (clamped to the definition's default budget).
    pub budget: Option<AgentRunBudget>,
    /// Workspace machine type (empty = no workspace).
    pub machine_type: String,
    /// Same key from the same starter returns the existing run.
    pub idempotency_key: String,
    /// What started it.
    pub trigger: Trigger,
    /// The trigger's subject.
    pub trigger_ref: String,
    /// A run id assigned upstream (issue assignment), or empty.
    pub run_id: String,
    /// The starter's delegation chain, starter first.
    pub chain: Vec<String>,
    /// The starter's token, when started through the API.
    pub parent_token: Option<String>,
    /// Watcher that triggered the run.
    pub watcher_id: String,
    /// Run this one retries.
    pub retry_of: String,
    /// Feed trigger that started the run (`<repo id>/<agent id>`).
    pub feed_trigger: String,
    /// ADOPT_EVALUATE: every post folded into the run, oldest first.
    pub feed_post_ids: Vec<String>,
    /// The reason the swarm lead gave for admitting this worker.
    pub swarm_join_reason: String,
}

/// A provenance session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    /// Session id.
    pub session: String,
    /// Run.
    pub run_id: String,
    /// Agent principal.
    pub agent_id: String,
    /// Model.
    pub model: String,
    /// Runtime name.
    pub runtime: String,
    /// BLAKE3 of the prompt.
    pub prompt_digest: Vec<u8>,
    /// Delegation chain, agent first.
    pub chain: Vec<String>,
    /// Changesets produced.
    pub changesets: Vec<ObjectId>,
    /// Change requests produced.
    pub change_request_ids: Vec<String>,
    /// Created (ms).
    pub created_ms: u64,
    /// Cost.
    pub cost: Cost,
}

fn session_of(s: &SessionRec) -> Session {
    Session {
        session: s.session.clone(),
        run_id: s.run_id.clone(),
        agent_id: s.agent_id.clone(),
        model: s.model.clone(),
        runtime: s.runtime.clone(),
        prompt_digest: s.prompt_digest.clone(),
        chain: s.chain.clone(),
        changesets: s.changesets.clone(),
        change_request_ids: s.change_request_ids.clone(),
        created_ms: s.created_ms,
        cost: s.cost.unwrap_or_default(),
    }
}

/// Field-wise minimum of two budgets where 0 means unlimited.
pub fn clamp_budget(req: &AgentRunBudget, max: &AgentRunBudget) -> AgentRunBudget {
    let m = |a: u64, b: u64| match (a, b) {
        (0, b) => b,
        (a, 0) => a,
        (a, b) => a.min(b),
    };
    AgentRunBudget {
        max_input_tokens: m(req.max_input_tokens, max.max_input_tokens),
        max_output_tokens: m(req.max_output_tokens, max.max_output_tokens),
        max_usd_micros: m(req.max_usd_micros, max.max_usd_micros),
        max_duration_seconds: m(req.max_duration_seconds, max.max_duration_seconds),
        max_changesets: m(req.max_changesets, max.max_changesets),
        max_landed: m(req.max_landed, max.max_landed),
    }
}

/// Which budget limit `cost` exceeds, if any.
pub fn over_budget(b: &AgentRunBudget, cost: &Cost) -> Option<&'static str> {
    if b.max_usd_micros > 0 && cost.usd_micros > b.max_usd_micros {
        Some("usd")
    } else if b.max_input_tokens > 0 && cost.input_tokens > b.max_input_tokens {
        Some("input_tokens")
    } else if b.max_output_tokens > 0 && cost.output_tokens > b.max_output_tokens {
        Some("output_tokens")
    } else {
        None
    }
}

pub(crate) fn terminal(s: i32) -> bool {
    matches!(
        St::try_from(s).unwrap_or(St::Unspecified),
        St::Succeeded | St::Failed | St::Cancelled
    )
}

fn run_row(rec: &RunRec) -> Row {
    let run = rec.run();
    let created = run.created_at.as_ref().map(crate::ids::ts_ms).unwrap_or(0);
    let mut row = Row::new(run_repo(&run), run.id.clone())
        .n1(created as i64)
        .msg(rec);
    if let Some(a) = &run.agent {
        row = row.k1(a.id.clone());
    }
    if !run.runtime_id.is_empty() {
        row = row.k2(run.runtime_id.clone());
    }
    row
}

pub(crate) async fn load_run(tx: &mut Tx, id: &str) -> Result<(RunRec, Row)> {
    let repo = locate(tx, "run", id)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("agent run {id}")))?;
    tx.get_msg::<RunRec>(&RUNS, &repo, id)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("agent run {id}")))
}

pub(crate) async fn save_run(tx: &mut Tx, rec: &RunRec, row: &Row) -> Result<u64> {
    tx.update(&RUNS, &run_row(rec).version(row.version)).await
}

/// Appends a session event to `rec` (the caller saves `rec`).
pub(crate) async fn push_event(
    tx: &mut Tx,
    rec: &mut RunRec,
    mut e: AgentRunEvent,
) -> Result<AgentRunEvent> {
    let mut run = rec.run();
    if run.last_event_seq >= MAX_RUN_EVENTS {
        return Ok(e);
    }
    run.last_event_seq += 1;
    e.run_id = run.id.clone();
    e.seq = run.last_event_seq;
    if e.time.is_none() {
        e.time = Some(ts(tx.now_ms()));
    }
    tx.insert(
        &RUN_EVENTS,
        &Row::new(run.id.clone(), pad(e.seq))
            .n1(e.seq as i64)
            .msg(&e),
    )
    .await?;
    rec.run = Some(run);
    Ok(e)
}

fn ev(kind: Ek) -> AgentRunEvent {
    AgentRunEvent {
        kind: kind as i32,
        ..Default::default()
    }
}

/// Moves `rec` to `state` with a `STATE_CHANGED` event.
pub(crate) async fn set_state(tx: &mut Tx, rec: &mut RunRec, state: St, text: &str) -> Result<()> {
    let mut run = rec.run();
    run.state = state as i32;
    let now = tx.now_ms();
    match state {
        St::Running if run.started_at.is_none() => run.started_at = Some(ts(now)),
        St::Succeeded | St::Failed | St::Cancelled => run.finished_at = Some(ts(now)),
        _ => {}
    }
    rec.run = Some(run);
    let mut e = ev(Ek::StateChanged);
    e.state = state as i32;
    e.text = text.to_string();
    push_event(tx, rec, e).await?;
    Ok(())
}

pub(crate) fn run_event(kind: &str, rec: &RunRec) -> Event {
    let run = rec.run();
    let repo = run_repo(&run);
    Event {
        kind: kind.to_string(),
        repo_id: repo.clone(),
        org_id: rec.org_id.clone(),
        subject: Some(SubjectRef {
            kind: SubjectKind::AgentRun as i32,
            repo: repo_ref_id(&repo),
            id: run.id.clone(),
        }),
        payload_json: super::pjson(&run),
        ..Default::default()
    }
}

pub(crate) fn run_audit(action: &str, rec: &RunRec) -> AuditDraft {
    let run = rec.run();
    AuditDraft::new(action, &rec.org_id)
        .repo_id(&run_repo(&run))
        .target(SubjectKind::AgentRun, &run.id)
        .after(
            &serde_json::json!({
                "run": run.id,
                "agent": run.agent.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
                "state": super::registry::state_name(run.state),
                "trigger": rec.trigger,
                "chain": rec.chain,
                "token_id": rec.token_id,
                "failure_reason": run.failure_reason,
            })
            .to_string(),
        )
}

fn task_action(task: &AgentTask) -> bee_policy::Action {
    match AgentTaskKind::try_from(task.kind).unwrap_or(AgentTaskKind::Unspecified) {
        AgentTaskKind::Review => bee_policy::Action::Review,
        _ => bee_policy::Action::Commit,
    }
}

fn check_task(task: &AgentTask) -> Result<()> {
    match AgentTaskKind::try_from(task.kind).unwrap_or(AgentTaskKind::Unspecified) {
        AgentTaskKind::Unspecified => Err(ForgeError::missing("task.kind")),
        AgentTaskKind::Issue if task.issue.trim().parse::<u64>().is_err() => Err(
            ForgeError::invalid("invalid_task", "an issue task needs an issue number"),
        ),
        AgentTaskKind::Review | AgentTaskKind::Revise
            if task.change_request.trim().parse::<u64>().is_err() =>
        {
            Err(ForgeError::invalid(
                "invalid_task",
                "review and revise tasks need a change request number",
            ))
        }
        AgentTaskKind::Freeform if task.prompt.trim().is_empty() => {
            Err(ForgeError::missing("task.prompt"))
        }
        AgentTaskKind::SecurityFix if task.alert.is_none() => {
            Err(ForgeError::missing("task.alert"))
        }
        AgentTaskKind::AdoptEvaluate if task.post_id.trim().is_empty() => {
            Err(ForgeError::missing("task.post_id"))
        }
        _ => check_len("task.prompt", &task.prompt, 64 * 1024),
    }
}

fn utf8(paths: &[Vec<u8>]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|p| crate::leases::normalize_path(p))
        .collect()
}

impl Agents {
    /// The agent the starter's runs use by default: the first enabled definition in the org
    /// that the starter owns.
    pub async fn default_agent(
        &self,
        cx: &Cx,
        org_id: &str,
        starter: &str,
    ) -> Result<AgentDefinition> {
        self.definitions(cx, org_id)
            .await?
            .into_iter()
            .find(|d| d.enabled && d.owners.iter().any(|o| o == starter))
            .ok_or_else(|| {
                ForgeError::not_found(format!("an agent owned by {starter} in this org"))
                    .with_subcode("no_default_agent")
            })
    }

    /// Starts (queues) a run. See the module docs for the token rules.
    pub async fn start_run(self: &Arc<Self>, cx: &Cx, req: StartRun) -> Result<AgentRun> {
        check_task(&req.task)?;
        let starter = req
            .chain
            .first()
            .cloned()
            .unwrap_or_else(|| cx.actor_id.clone());
        if !req.idempotency_key.is_empty() {
            check_len("idempotency_key", &req.idempotency_key, 256)?;
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if let Some(id) =
                locate(&mut tx, &format!("idem:{starter}"), &req.idempotency_key).await?
            {
                let (rec, _) = load_run(&mut tx, &id).await?;
                return Ok(rec.run());
            }
        }
        if !req.run_id.is_empty() {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if locate(&mut tx, "run", &req.run_id).await?.is_some() {
                let (rec, _) = load_run(&mut tx, &req.run_id).await?;
                return Ok(rec.run());
            }
        }
        let def = if req.agent_id.is_empty() {
            self.default_agent(cx, &req.repo.org_id, &starter).await?
        } else {
            self.definition(cx, &req.repo.org_id, &req.agent_id).await?
        };
        if !def.enabled {
            return Err(
                ForgeError::invalid_state(format!("agent {} is disabled", def.name))
                    .with_subcode("agent_disabled"),
            );
        }
        if req.trigger == Trigger::SwarmJoin && def.owner() != starter {
            return Err(
                ForgeError::permission_denied("The joining agent changed owners")
                    .with_subcode("swarm_join_owner"),
            );
        }
        let mut runtime_id = def.runtime_id.clone();
        let mut runtime_name = def.runtime.clone();
        let mut default_model = String::new();
        if !req.runtime_id.is_empty() {
            runtime_id = req.runtime_id.clone();
        }
        if !runtime_id.is_empty() {
            let rt = self.get_runtime(cx, &runtime_id).await?;
            if !rt.org.is_empty() && rt.org != req.repo.org_id {
                return Err(ForgeError::not_found(format!("runtime {runtime_id}")));
            }
            if !rt.enabled {
                return Err(ForgeError::invalid_state(format!(
                    "runtime {} is disabled",
                    rt.name
                )));
            }
            runtime_name = rt.name.clone();
            default_model = rt.default_models.first().cloned().unwrap_or_default();
        }
        // Scope: never broader than the definition's toolsets.
        let toolsets: BTreeSet<String> = def.toolsets.iter().cloned().collect();
        let mut actions: Vec<String> = if req.scope.actions.is_empty() {
            def.toolsets.clone()
        } else {
            req.scope
                .actions
                .iter()
                .map(|a| a.trim().to_string())
                .collect()
        };
        actions.sort();
        actions.dedup();
        for a in &actions {
            bee_policy::Action::parse(a).ok_or_else(|| {
                ForgeError::invalid("invalid_scope", format!("unknown action {a:?}"))
            })?;
            if !toolsets.contains(a) {
                return Err(ForgeError::permission_denied(format!(
                    "the scope asks for `{a}`, which agent {} may not use",
                    def.name
                ))
                .with_subcode("scope_too_broad"));
            }
        }
        let need = task_action(&req.task);
        if !actions.iter().any(|a| a == need.as_str()) {
            return Err(ForgeError::permission_denied(format!(
                "this task needs `{need}`, which the run's scope does not allow"
            ))
            .with_subcode("scope_too_narrow"));
        }
        let read_paths = utf8(&req.scope.read_paths)?;
        let write_paths = utf8(&req.scope.write_paths)?;
        let budget = clamp_budget(&req.budget.unwrap_or_default(), &def.default_budget);
        let base_ref = if req.base_ref.trim().is_empty() {
            req.repo.default_ref.clone()
        } else {
            req.base_ref.trim().to_string()
        };
        let run_id = if req.run_id.is_empty() {
            self.db.ids().ulid()?
        } else {
            req.run_id.clone()
        };
        let agent = def.agent.clone();
        let work_ref = format!("ws/{}/{}", agent.id, run_id.to_lowercase());
        let refs: Vec<String> = if req.trigger == Trigger::SwarmJoin {
            vec![work_ref.clone(), base_ref.clone()]
        } else if req.scope.refs.is_empty() {
            vec![format!("ws/{}/*", agent.id), base_ref.clone()]
        } else {
            req.scope.refs.clone()
        };
        let session = format!("run-{}", run_id.to_lowercase());
        // Delegation chain: owner first, then the starter's chain, without the agent itself.
        let owner = def.owner().to_string();
        let mut chain_ids: Vec<String> = vec![owner.clone()];
        for p in &req.chain {
            if *p != agent.id && !chain_ids.contains(p) {
                chain_ids.push(p.clone());
            }
        }
        let mut on_behalf_of = Vec::new();
        for id in &chain_ids {
            let kind = match self.forge.hooks.directory.get(id).await {
                Some(p) if p.kind == PrincipalKind::Agent as i32 => {
                    bee_policy::PrincipalKind::Agent
                }
                Some(p) if p.kind == PrincipalKind::Service as i32 => {
                    bee_policy::PrincipalKind::Service
                }
                _ => bee_policy::PrincipalKind::Human,
            };
            on_behalf_of.push(PrincipalRef::new(id.clone(), kind));
        }
        let now = self.db.now_ms();
        let ttl = if budget.max_duration_seconds > 0 {
            budget.max_duration_seconds + 3600
        } else {
            self.config.run_token_ttl.as_secs()
        };
        let mut scope = Scope::new()
            .repos([req.repo.id.as_str()])?
            .refs(refs.iter())?
            .actions(actions.iter().filter_map(|a| bee_policy::Action::parse(a)))
            .session(session.clone())
            .expires_at(now / 1000 + ttl.max(60));
        if !read_paths.is_empty() {
            scope = scope.read_paths(read_paths.iter().map(|p| format!("/{p}")))?;
        }
        if !write_paths.is_empty() {
            scope = scope.write_paths(write_paths.iter().map(|p| format!("/{p}")))?;
        }
        if budget.max_changesets > 0 || budget.max_landed > 0 {
            scope = scope.budget(Budget {
                max_changesets: (budget.max_changesets > 0).then_some(budget.max_changesets),
                max_landed: (budget.max_landed > 0).then_some(budget.max_landed),
                max_upload_bytes: None,
            });
        }
        let model = if !req.model.trim().is_empty() {
            req.model.trim().to_string()
        } else if !def.model.is_empty() {
            def.model.clone()
        } else {
            default_model
        };
        let run = AgentRun {
            id: run_id.clone(),
            repo: repo_ref_id(&req.repo.id),
            task: Some(req.task.clone()),
            runtime_id: runtime_id.clone(),
            model,
            agent: Some(agent.clone()),
            started_by: self.forge.hooks.directory.get(&starter).await.or_else(|| {
                Some(Principal {
                    id: starter.clone(),
                    ..Default::default()
                })
            }),
            state: St::Queued as i32,
            base_ref: base_ref.clone(),
            work_ref,
            scope: Some(AgentRunScope {
                read_paths: req.scope.read_paths.clone(),
                write_paths: req.scope.write_paths.clone(),
                refs,
                actions: actions.clone(),
            }),
            budget: Some(budget),
            cost: Some(Cost::default()),
            session: session.clone(),
            created_at: Some(ts(now)),
            url: format!(
                "{}/{}/agents/runs/{}",
                self.config.base_url, req.repo.full_name, run_id
            ),
            ..Default::default()
        };
        let mut rec = RunRec {
            run: Some(run),
            org_id: req.repo.org_id.clone(),
            trigger: req.trigger.as_str().to_string(),
            trigger_ref: req.trigger_ref.clone(),
            chain: std::iter::once(agent.id.clone())
                .chain(chain_ids.iter().cloned())
                .collect(),
            issue_number: if req.task.kind == AgentTaskKind::Issue as i32 {
                req.task.issue.trim().parse().unwrap_or(0)
            } else {
                0
            },
            instructions_path: def.instructions_path.clone(),
            toolsets: def.toolsets.clone(),
            watcher_id: req.watcher_id.clone(),
            retry_of: req.retry_of.clone(),
            feed_trigger: req.feed_trigger.clone(),
            feed_post_ids: req.feed_post_ids.clone(),
            idempotency_key: req.idempotency_key.clone(),
            owner_id: owner.clone(),
            runtime_name: runtime_name.clone(),
            machine_type: req.machine_type.clone(),
            ..Default::default()
        };
        // Budget first: a refused run never gets a token.
        let refusal = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            ensure_budget(&mut tx, &req.repo.org_id, &owner).await.err()
        };
        if let Some(err) = refusal {
            return self.refuse(cx, &req, rec, err).await;
        }
        let mut issued_token_id = None;
        let out = async {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            // Race starts against Stop Everything before minting a run credential.
            crate::safety::control::require_running_in_tx(&mut tx, &req.repo.org_id).await?;
            if !req.idempotency_key.is_empty() {
                let kind = format!("idem:{starter}");
                if let Err(error) =
                    insert_locator(&mut tx, &kind, &req.idempotency_key, &run_id).await
                {
                    if error.reason != bee_proto::v1::ErrorReason::AlreadyExists {
                        return Err(error);
                    }
                    // A duplicate insert aborts a PostgreSQL transaction. Roll it back before
                    // loading the winner, and avoid issuing a token to the losing start.
                    drop(tx);
                    let mut read_tx = self.db.begin(SURFACE, cx).await?;
                    let existing = locate(&mut read_tx, &kind, &req.idempotency_key)
                        .await?
                        .ok_or_else(|| ForgeError::already_exists("idempotency key in use"))?;
                    let (existing, _) = load_run(&mut read_tx, &existing).await?;
                    return Ok(existing.run());
                }
            }
            if req.trigger == Trigger::SwarmJoin {
                // Membership and the queued run must commit together. Admission also takes
                // the swarm lock shared with pause, stop and other join decisions.
                crate::swarms::store::admit(
                    &mut tx,
                    &req.repo,
                    &req.trigger_ref,
                    &req.swarm_join_reason,
                    &rec.run(),
                )
                .await?;
            }
            let issued = self
                .ports
                .issuer
                .issue(&RunGrant {
                    run_id: run_id.clone(),
                    agent: agent.clone(),
                    owner_id: owner.clone(),
                    runtime: runtime_name.clone(),
                    purpose: def.purpose.clone(),
                    on_behalf_of,
                    parent_token: req.parent_token.clone(),
                    parent_principal: starter.clone(),
                    scope,
                })
                .await?;
            issued_token_id = Some(issued.token_id.clone());
            rec.token_id = issued.token_id.clone();
            let mut e = ev(Ek::StateChanged);
            e.state = St::Queued as i32;
            e.text = format!("queued by {}", req.trigger.as_str());
            push_event(&mut tx, &mut rec, e).await?;
            tx.insert(&RUNS, &run_row(&rec)).await?;
            put_locator(&mut tx, "run", &run_id, &req.repo.id).await?;
            tx.insert(
                &RUN_TOKENS,
                &Row::new(run_id.clone(), "token").msg(&RunTokenRec {
                    token: issued.token.clone(),
                    token_id: issued.token_id.clone(),
                }),
            )
            .await?;
            tx.insert(
                &QUEUE,
                &Row::new("", run_id.clone())
                    .k1(req.repo.id.clone())
                    .n1(now as i64),
            )
            .await?;
            charge(&mut tx, &req.repo.org_id, &owner, &Cost::default(), 1).await?;
            tx.emit_event(run_event("agent_run.queued", &rec)).await?;
            tx.audit(run_audit("agent.run_start", &rec)).await?;
            tx.commit().await?;
            Ok(rec.run())
        }
        .await;
        match out {
            Ok(run) => {
                self.wake.notify_one();
                Ok(run)
            }
            Err(e) => {
                if let Some(token_id) = issued_token_id {
                    let _ = self.ports.issuer.revoke(&token_id).await;
                }
                Err(e)
            }
        }
    }

    /// A run refused for budget: triggered runs are recorded as `FAILED` (there is nobody to
    /// answer); API starts get the error. Both are audited.
    async fn refuse(
        &self,
        cx: &Cx,
        req: &StartRun,
        mut rec: RunRec,
        err: ForgeError,
    ) -> Result<AgentRun> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if matches!(
            req.trigger,
            Trigger::Manual | Trigger::Retry | Trigger::SwarmJoin
        ) {
            tx.audit(run_audit("agent.run_refused", &rec).result(err.reason_name()))
                .await?;
            tx.commit().await?;
            return Err(err);
        }
        let mut run = rec.run();
        run.failure_reason = err.reason_name().to_string();
        run.summary = err.message.clone();
        rec.run = Some(run);
        set_state(&mut tx, &mut rec, St::Failed, &err.message).await?;
        tx.insert(&RUNS, &run_row(&rec)).await?;
        put_locator(&mut tx, "run", &rec.run().id, &req.repo.id).await?;
        tx.emit_event(run_event("agent_run.refused", &rec)).await?;
        tx.audit(run_audit("agent.run_refused", &rec).result(err.reason_name()))
            .await?;
        tx.commit().await?;
        if rec.issue_number > 0 {
            let _ = self
                .forge
                .work
                .finish_agent_run(
                    &Cx::system(),
                    &RepoInfo::from(&req.repo),
                    rec.issue_number,
                    &rec.run().id,
                    "budget exhausted",
                )
                .await;
        }
        Ok(rec.run())
    }

    /// A run by id.
    pub async fn get_run(&self, cx: &Cx, id: &str) -> Result<AgentRun> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(load_run(&mut tx, id).await?.0.run())
    }

    pub(crate) async fn run_rec(&self, cx: &Cx, id: &str) -> Result<RunRec> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(load_run(&mut tx, id).await?.0)
    }

    /// Runs filtered by repo (or every repo in `repo_ids`), agent, states, task kind and
    /// starter; newest first, paged by run id.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_runs(
        &self,
        cx: &Cx,
        repo_ids: &[String],
        agent_id: &str,
        states: &[i32],
        task_kind: i32,
        started_by: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<AgentRun>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut rows: Vec<RunRec> = Vec::new();
        if !agent_id.is_empty() {
            for (r, row) in tx
                .scan_msgs::<RunRec>(
                    &RUNS,
                    &Scan {
                        key: Some(crate::store::Key::K1(agent_id.to_string())),
                        limit: crate::store::MAX_SCAN,
                        ..Scan::default()
                    },
                )
                .await?
            {
                if repo_ids.contains(&row.scope) {
                    rows.push(r);
                }
            }
        } else {
            for repo in repo_ids {
                for (r, _) in tx
                    .scan_msgs::<RunRec>(&RUNS, &Scan::scope(repo.clone(), crate::store::MAX_SCAN))
                    .await?
                {
                    rows.push(r);
                }
            }
        }
        let mut runs: Vec<AgentRun> = rows
            .into_iter()
            .map(|r| r.run())
            .filter(|r| states.is_empty() || states.contains(&r.state))
            .filter(|r| task_kind == 0 || r.task.as_ref().is_some_and(|t| t.kind == task_kind))
            .filter(|r| {
                started_by.is_empty() || r.started_by.as_ref().is_some_and(|p| p.id == started_by)
            })
            .collect();
        runs.sort_by(|a, b| b.id.cmp(&a.id));
        let mut v: Vec<AgentRun> = runs
            .into_iter()
            .filter(|r| after.is_empty() || r.id.as_str() < after)
            .collect();
        let more = v.len() > limit;
        v.truncate(limit);
        let next = if more {
            v.last().map(|r| r.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((v, next))
    }

    /// Session events of a run after `after_seq`, filtered by kind, at most `limit`.
    pub async fn run_events(
        &self,
        cx: &Cx,
        run_id: &str,
        after_seq: u64,
        kinds: &[i32],
        limit: usize,
    ) -> Result<Vec<AgentRunEvent>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut out = Vec::new();
        let mut from = after_seq as i64 + 1;
        loop {
            let rows = tx
                .scan_msgs::<AgentRunEvent>(
                    &RUN_EVENTS,
                    &Scan::scope(run_id, 1000).by_n1().n1_min(from),
                )
                .await?;
            let n = rows.len();
            for (e, _) in rows {
                from = e.seq as i64 + 1;
                if kinds.is_empty() || kinds.contains(&e.kind) {
                    out.push(e);
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
            if n < 1000 {
                return Ok(out);
            }
        }
    }

    /// Cancels a queued, running or waiting run.
    pub async fn cancel_run(&self, cx: &Cx, id: &str, reason: &str) -> Result<AgentRun> {
        check_len("reason", reason, 4096)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((rec, was_running)) = self.cancel_in_tx(cx, &mut tx, id, reason).await? else {
            return Err(ForgeError::invalid_state(format!(
                "run {id} already finished"
            )));
        };
        tx.commit().await?;
        self.after_cancel(&rec, was_running).await;
        Ok(rec.run())
    }

    // Swarm stop uses the ordinary cancellation path for all members in one transaction.
    pub(crate) async fn cancel_in_tx(
        &self,
        cx: &Cx,
        tx: &mut Tx,
        id: &str,
        reason: &str,
    ) -> Result<Option<(RunRec, bool)>> {
        let repo = locate(tx, "run", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("agent run {id}")))?;
        let row = tx
            .get_for_update(&RUNS, &repo, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("agent run {id}")))?;
        let mut rec: RunRec = row.decode()?;
        let run = rec.run();
        if terminal(run.state) {
            return Ok(None);
        }
        let was_running = run.state == St::Running as i32;
        let mut r = rec.run();
        r.failure_reason = "CANCELLED".into();
        if !reason.is_empty() {
            r.summary = reason.to_string();
        }
        rec.run = Some(r);
        set_state(
            tx,
            &mut rec,
            St::Cancelled,
            &format!(
                "cancelled by {}{}",
                cx.actor_id,
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(": {reason}")
                }
            ),
        )
        .await?;
        save_run(tx, &rec, &row).await?;
        tx.delete(&QUEUE, "", id).await?;
        tx.delete(&RUN_TOKENS, id, "token").await?;
        tx.emit_event(run_event("agent_run.completed", &rec))
            .await?;
        tx.audit(run_audit("agent.run_cancel", &rec)).await?;
        Ok(Some((rec, was_running)))
    }

    pub(crate) async fn after_cancel(&self, rec: &RunRec, was_running: bool) {
        self.run_done.notify_waiters();
        if was_running {
            let _ = self.ports.runner.cancel(&rec.run().id).await;
        }
        self.after_finish(rec, "cancelled").await;
    }

    /// Starts a new run with a finished run's task.
    pub async fn retry_run(
        self: &Arc<Self>,
        cx: &Cx,
        id: &str,
        runtime_id: &str,
        budget: Option<AgentRunBudget>,
        chain: Vec<String>,
        parent_token: Option<String>,
    ) -> Result<AgentRun> {
        let rec = self.run_rec(cx, id).await?;
        let run = rec.run();
        if !terminal(run.state) {
            return Err(ForgeError::invalid_state(format!(
                "run {id} has not finished"
            )));
        }
        let repo = self.forge.orgs.repo_any(cx, &run_repo(&run)).await?;
        self.start_run(
            cx,
            StartRun {
                repo,
                task: run.task.clone().unwrap_or_default(),
                agent_id: run.agent.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
                runtime_id: if runtime_id.is_empty() {
                    run.runtime_id.clone()
                } else {
                    runtime_id.to_string()
                },
                model: run.model.clone(),
                base_ref: run.base_ref.clone(),
                scope: run.scope.clone().unwrap_or_default(),
                budget: budget.or(run.budget),
                machine_type: rec.machine_type.clone(),
                trigger: Trigger::Retry,
                trigger_ref: id.to_string(),
                chain,
                parent_token,
                retry_of: id.to_string(),
                watcher_id: rec.watcher_id.clone(),
                feed_trigger: rec.feed_trigger.clone(),
                feed_post_ids: rec.feed_post_ids.clone(),
                ..StartRun::default()
            },
        )
        .await
    }

    /// Posts a steering message to a live run.
    pub async fn send_message(&self, cx: &Cx, id: &str, body: &str) -> Result<AgentRunEvent> {
        if body.trim().is_empty() {
            return Err(ForgeError::missing("body"));
        }
        check_len("body", body, MAX_MESSAGE)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let repo_id = locate(&mut tx, "run", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("agent run {id}")))?;
        crate::swarms::store::guard_message(&mut tx, &repo_id, id).await?;
        let (mut rec, row) = load_run(&mut tx, id).await?;
        let run = rec.run();
        if terminal(run.state) {
            return Err(ForgeError::invalid_state(format!(
                "run {id} already finished"
            )));
        }
        let live = run.state == St::Running as i32 || run.state == St::WaitingForReview as i32;
        if !live {
            rec.pending_messages.push(body.to_string());
        }
        let mut e = ev(Ek::SteeringMessage);
        e.text = body.to_string();
        e.author = Some(cx.principal());
        let e = push_event(&mut tx, &mut rec, e).await?;
        save_run(&mut tx, &rec, &row).await?;
        tx.emit_event(run_event("agent_run.message", &rec)).await?;
        tx.audit(run_audit("agent.run_message", &rec)).await?;
        tx.commit().await?;
        self.run_done.notify_waiters();
        if live {
            self.ports.runner.steer(id, body).await?;
        }
        Ok(e)
    }

    /// Sessions that produced a changeset or a change request in a repo.
    pub async fn sessions_for_change(
        &self,
        cx: &Cx,
        repo_id: &str,
        changeset: Option<&ObjectId>,
        change_request_id: Option<&str>,
    ) -> Result<Vec<Session>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut keys = Vec::new();
        if let Some(cs) = changeset {
            keys.push(format!("cs:{}", hex(&cs.raw)));
        }
        if let Some(cr) = change_request_id {
            keys.push(format!("cr:{cr}"));
        }
        let mut out: Vec<Session> = Vec::new();
        for k in keys {
            if let Some(row) = tx.get(&SESSION_LINKS, repo_id, &k).await?
                && let Some(sid) = row.k1
                && let Some((s, _)) = tx.get_msg::<SessionRec>(&SESSIONS, repo_id, &sid).await?
                && !out.iter().any(|x| x.session == s.session)
            {
                out.push(session_of(&s));
            }
        }
        Ok(out)
    }

    /// A session by id.
    pub async fn session(&self, cx: &Cx, repo_id: &str, session: &str) -> Result<Session> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<SessionRec>(&SESSIONS, repo_id, session)
            .await?
            .map(|(s, _)| session_of(&s))
            .ok_or_else(|| ForgeError::not_found(format!("session {session}")))
    }

    // ---- dispatch ----

    pub(crate) async fn run_queued(self: &Arc<Self>, cx: &Cx) -> Result<usize> {
        let limit = self.config.max_concurrent_runs.max(1);
        let mut claimed = Vec::new();
        let mut cursor = None;
        // Paused swarms retain their rows. Walk past them so they do not hide runnable work.
        while claimed.len() < limit {
            let queued = {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                tx.scan(&QUEUE, &Scan::scope("", 128).by_n1().after(cursor))
                    .await?
            };
            cursor = queued.last().map(crate::store::Cursor::after);
            let done = queued.len() < 128;
            for q in queued {
                if let Some(id) = self.claim(cx, &q.id).await? {
                    claimed.push(id);
                    if claimed.len() == limit {
                        break;
                    }
                }
            }
            if done {
                break;
            }
        }
        let n = claimed.len();
        let mut set = JoinSet::new();
        for id in claimed {
            let me = self.clone();
            set.spawn(async move { me.execute(&id).await });
        }
        while let Some(r) = set.join_next().await {
            if let Ok(Err(e)) = r {
                return Err(e);
            }
        }
        Ok(n)
    }

    async fn claim(&self, cx: &Cx, id: &str) -> Result<Option<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(q) = tx.get(&QUEUE, "", id).await? else {
            return Ok(None);
        };
        let repo = q.k1.as_deref().unwrap_or_default();
        if !crate::swarms::store::may_dispatch(&mut tx, repo, id).await? {
            return Ok(None);
        }
        let Some(row) = tx.get_for_update(&RUNS, repo, id).await? else {
            tx.delete(&QUEUE, "", id).await?;
            tx.commit().await?;
            return Ok(None);
        };
        if !tx.delete(&QUEUE, "", id).await? {
            return Ok(None);
        }
        let mut rec: RunRec = row.decode()?;
        if rec.run().state != St::Queued as i32 {
            tx.commit().await?;
            return Ok(None);
        }
        set_state(
            &mut tx,
            &mut rec,
            St::Provisioning,
            "claimed by the dispatcher",
        )
        .await?;
        save_run(&mut tx, &rec, &row).await?;
        tx.emit_event(run_event("agent_run.state_changed", &rec))
            .await?;
        tx.audit(run_audit("agent.run_claim", &rec)).await?;
        tx.commit().await?;
        Ok(Some(id.to_string()))
    }

    /// Executes one claimed run to its next resting state.
    async fn execute(self: &Arc<Self>, id: &str) -> Result<()> {
        let cx = Cx::system();
        let (rec, token) = {
            let mut tx = self.db.begin(SURFACE, &cx).await?;
            let (rec, _) = load_run(&mut tx, id).await?;
            let token = tx
                .get_msg::<RunTokenRec>(&RUN_TOKENS, id, "token")
                .await?
                .map(|(t, _)| t.token)
                .unwrap_or_default();
            (rec, token)
        };
        let run = rec.run();
        if run.state != St::Provisioning as i32 {
            return Ok(());
        }
        let prepared = self.prepare(&cx, &rec, token).await;
        let job = match prepared {
            Ok(j) => j,
            Err(e) => {
                self.finish(
                    id,
                    RunOutcome {
                        status: RunStatus::Failed(e.reason_name().to_string()),
                        summary: e.message.clone(),
                        ..RunOutcome::default()
                    },
                )
                .await?;
                return Ok(());
            }
        };
        // RUNNING, with the provenance session.
        {
            let mut tx = self.db.begin(SURFACE, &cx).await?;
            let (mut rec, row) = load_run(&mut tx, id).await?;
            if rec.run().state != St::Provisioning as i32 {
                return Ok(());
            }
            let mut r = rec.run();
            r.workspace_id = job
                .workspace
                .as_ref()
                .map(|w| w.id.clone())
                .unwrap_or_default();
            rec.run = Some(r);
            rec.prompt_digest =
                blake3::hash(format!("{}\n---\n{}", job.instructions, job.prompt).as_bytes())
                    .as_bytes()
                    .to_vec();
            rec.pending_messages.clear();
            set_state(&mut tx, &mut rec, St::Running, "running").await?;
            save_run(&mut tx, &rec, &row).await?;
            let run = rec.run();
            let repo = run_repo(&run);
            let srec = SessionRec {
                session: run.session.clone(),
                repo_id: repo.clone(),
                org_id: rec.org_id.clone(),
                agent_id: run.agent.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
                run_id: run.id.clone(),
                model: run.model.clone(),
                runtime: rec.runtime_name.clone(),
                prompt_digest: rec.prompt_digest.clone(),
                chain: rec.chain.clone(),
                created_ms: tx.now_ms(),
                cost: Some(Cost::default()),
                ..Default::default()
            };
            tx.put(
                &SESSIONS,
                &Row::new(repo, run.session.clone())
                    .k1(srec.agent_id.clone())
                    .n1(srec.created_ms as i64)
                    .msg(&srec),
            )
            .await?;
            tx.emit_event(run_event("agent_run.started", &rec)).await?;
            tx.audit(run_audit("agent.run_running", &rec)).await?;
            tx.commit().await?;
            self.run_done.notify_waiters();
        }
        let reporter: Arc<dyn RunReporter> = Arc::new(Reporter {
            agents: self.clone(),
            run_id: id.to_string(),
        });
        let mut job = job;
        if let Ok(r) = self.get_run(&cx, id).await {
            job.run = r;
        }
        let budget = job.run.budget.unwrap_or_default();
        let fut = self.ports.runner.execute(job, reporter);
        let outcome = if budget.max_duration_seconds > 0 {
            match tokio::time::timeout(
                std::time::Duration::from_secs(budget.max_duration_seconds),
                fut,
            )
            .await
            {
                Ok(r) => r,
                Err(_) => {
                    let _ = self.ports.runner.cancel(id).await;
                    Ok(RunOutcome {
                        status: RunStatus::Failed("DEADLINE_EXCEEDED".into()),
                        summary: "the run exceeded its time budget".into(),
                        ..RunOutcome::default()
                    })
                }
            }
        } else {
            fut.await
        };
        let outcome = outcome.unwrap_or_else(|e| RunOutcome {
            status: RunStatus::Failed(e.reason_name().to_string()),
            summary: e.message,
            ..RunOutcome::default()
        });
        self.finish(id, outcome).await
    }

    /// Builds the runner's job: definition, runtime, instructions, task context, workspace.
    async fn prepare(&self, cx: &Cx, rec: &RunRec, token: String) -> Result<RunJob> {
        let run = rec.run();
        let repo = self.forge.orgs.repo_any(cx, &run_repo(&run)).await?;
        let agent_id = run.agent.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let def = self.definition(cx, &rec.org_id, &agent_id).await?;
        if !def.enabled {
            return Err(ForgeError::invalid_state("the agent was disabled"));
        }
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            ensure_budget(&mut tx, &rec.org_id, &rec.owner_id).await?;
        }
        let runtime = if run.runtime_id.is_empty() {
            None
        } else {
            self.get_runtime(cx, &run.runtime_id).await.ok()
        };
        let task = run.task.clone().unwrap_or_default();
        let kind = AgentTaskKind::try_from(task.kind).unwrap_or(AgentTaskKind::Unspecified);
        // Instructions: repo instruction files plus the definition's file.
        let mut instructions = String::new();
        if let Ok(base) = self.ports.repo.resolve(&repo, &run.base_ref).await {
            if let Ok(res) = self
                .resolve_instructions(cx, &repo, &base, &[], &rec.runtime_name, "")
                .await
            {
                instructions = res.combined;
            }
            if !def.instructions_path.is_empty()
                && let Ok(Some((_, bytes))) = self
                    .ports
                    .repo
                    .read_file(&repo, &base, &def.instructions_path)
                    .await
            {
                instructions.push_str(&format!(
                    "\n\n<!-- {} -->\n{}",
                    def.instructions_path,
                    String::from_utf8_lossy(&bytes)
                ));
            }
        }
        let mut issue = None;
        let mut change_request = None;
        let mut prompt = String::new();
        match kind {
            AgentTaskKind::Issue => {
                let n: u64 = task.issue.trim().parse().unwrap_or(0);
                let i = self
                    .forge
                    .work
                    .get_issue_by_number(cx, &RepoInfo::from(&repo), n)
                    .await?;
                prompt = format!(
                    "Resolve issue #{} in {}: {}\n\n{}\n\nOpen a change request that links issue #{}.",
                    i.number, repo.full_name, i.title, i.body, i.number
                );
                issue = Some(i);
            }
            AgentTaskKind::Review | AgentTaskKind::Revise => {
                let n: u64 = task.change_request.trim().parse().unwrap_or(0);
                let cr = self.changes.get_by_number(cx, &repo, n).await?;
                prompt = if kind == AgentTaskKind::Review {
                    format!(
                        "Review change request #{} in {}: {}\n\n{}",
                        cr.number, repo.full_name, cr.title, cr.body
                    )
                } else {
                    format!(
                        "Address the review comments on your change request #{} in {}: {}",
                        cr.number, repo.full_name, cr.title
                    )
                };
                change_request = Some(cr);
            }
            AgentTaskKind::AdoptEvaluate => {
                prompt = self.adopt_context(rec, &repo).await?;
            }
            AgentTaskKind::SecurityFix => {
                let a = task.alert.clone().unwrap_or_default();
                prompt = format!(
                    "Fix security alert {} in {} and open a change request.",
                    a.id, repo.full_name
                );
            }
            _ => {}
        }
        if !task.prompt.is_empty() {
            if !prompt.is_empty() {
                prompt.push_str("\n\nGuidance:\n");
            }
            prompt.push_str(&task.prompt);
        }
        for b in &task.context_bundle_ids {
            if let Ok(m) = self.materialize_bundle(cx, b, None, false, 200).await {
                prompt.push_str(&format!("\n\nContext bundle {b}:"));
                for it in &m.items {
                    for f in &it.files {
                        prompt.push_str(&format!("\n- {}", String::from_utf8_lossy(&f.path)));
                    }
                    if !it.title.is_empty() {
                        prompt.push_str(&format!("\n- {}", it.title));
                    }
                }
            }
        }
        let workspace = if rec.machine_type.is_empty() {
            None
        } else {
            Some(
                self.workspaces
                    .create_for_run(cx, &repo, &def.agent, &run, &rec.machine_type)
                    .await?,
            )
        };
        let budget = run.budget.unwrap_or_default();
        Ok(RunJob {
            deadline_ms: if budget.max_duration_seconds > 0 {
                self.db.now_ms() + budget.max_duration_seconds * 1000
            } else {
                0
            },
            run,
            repo,
            token,
            chain: rec.chain.clone(),
            definition: def,
            runtime,
            instructions,
            prompt,
            issue,
            change_request,
            workspace,
            messages: rec.pending_messages.clone(),
        })
    }

    /// Records a runner's outcome and moves the run to its resting state.
    pub(crate) async fn finish(self: &Arc<Self>, id: &str, o: RunOutcome) -> Result<()> {
        let cx = Cx::system();
        let log_ref = if o.log.is_empty() {
            String::new()
        } else {
            self.ports.blobs.put(&o.log).await?
        };
        let transcript_ref = if o.transcript.is_empty() {
            String::new()
        } else {
            self.ports.blobs.put(&o.transcript).await?
        };
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        let (mut rec, row) = load_run(&mut tx, id).await?;
        let mut run = rec.run();
        let cancelled = run.state == St::Cancelled as i32;
        if terminal(run.state) && !cancelled {
            return Ok(());
        }
        let repo_id = run_repo(&run);
        for cs in &o.changesets {
            if !run.changesets.contains(cs) {
                run.changesets.push(cs.clone());
            }
        }
        for n in &o.change_requests {
            let s = n.to_string();
            if !run.change_requests.contains(&s) {
                run.change_requests.push(s);
            }
        }
        for l in &o.lease_ids {
            if !run.lease_ids.contains(l) {
                run.lease_ids.push(l.clone());
            }
        }
        let before = run.cost.unwrap_or_default();
        let mut cost = before;
        if o.cost.usd_micros >= cost.usd_micros
            && o.cost.input_tokens >= cost.input_tokens
            && o.cost.output_tokens >= cost.output_tokens
        {
            cost = o.cost;
        }
        // Live cost updates only record; the owner is charged once, here, for the whole run.
        let delta = cost;
        run.cost = Some(cost);
        if !o.model.is_empty() {
            run.model = o.model.clone();
        }
        if !o.summary.is_empty() && !cancelled {
            run.summary = o.summary.clone();
        }
        let budget = run.budget.unwrap_or_default();
        let over = over_budget(&budget, &cost).is_some() || rec.over_budget;
        let open_crs = !run.change_requests.is_empty();
        let (state, reason) = if cancelled {
            (St::Cancelled, run.failure_reason.clone())
        } else if over {
            (St::Failed, "BUDGET_EXHAUSTED".to_string())
        } else {
            match &o.status {
                RunStatus::Succeeded if open_crs => (St::WaitingForReview, String::new()),
                RunStatus::Succeeded => (St::Succeeded, String::new()),
                RunStatus::Failed(r) => (St::Failed, r.clone()),
                RunStatus::Cancelled => (St::Cancelled, "CANCELLED".to_string()),
            }
        };
        run.failure_reason = reason.clone();
        rec.run = Some(run.clone());
        rec.log_ref = log_ref.clone();
        rec.transcript_ref = transcript_ref.clone();
        if !log_ref.is_empty() || !transcript_ref.is_empty() {
            let mut e = ev(Ek::Log);
            e.text = "transcript stored".into();
            e.tool_name = "transcript".into();
            e.tool_payload =
                serde_json::json!({"log_ref": log_ref, "transcript_ref": transcript_ref})
                    .to_string();
            push_event(&mut tx, &mut rec, e).await?;
        }
        let mut ce = ev(Ek::CostUpdate);
        ce.cost = Some(cost);
        push_event(&mut tx, &mut rec, ce).await?;
        if !cancelled {
            set_state(&mut tx, &mut rec, state, &reason).await?;
        }
        save_run(&mut tx, &rec, &row).await?;
        // Session: changesets, change requests, cost, and the links that make it queryable by
        // change.
        let run = rec.run();
        if let Some((mut s, srow)) = tx
            .get_msg::<SessionRec>(&SESSIONS, &repo_id, &run.session)
            .await?
        {
            for cs in &run.changesets {
                if !s.changesets.contains(cs) {
                    s.changesets.push(cs.clone());
                }
                tx.put(
                    &SESSION_LINKS,
                    &Row::new(repo_id.clone(), format!("cs:{}", hex(&cs.raw)))
                        .k1(run.session.clone()),
                )
                .await?;
            }
            s.cost = Some(cost);
            s.model = run.model.clone();
            tx.update(
                &SESSIONS,
                &Row::new(repo_id.clone(), run.session.clone())
                    .k1(s.agent_id.clone())
                    .n1(s.created_ms as i64)
                    .version(srow.version)
                    .msg(&s),
            )
            .await?;
        }
        charge(&mut tx, &rec.org_id, &rec.owner_id, &delta, 0).await?;
        if delta.input_tokens + delta.output_tokens > 0 {
            tx.emit_event(Event {
                kind: "usage.recorded".into(),
                repo_id: repo_id.clone(),
                org_id: rec.org_id.clone(),
                principal: rec.owner_id.clone(),
                subject: Some(SubjectRef {
                    kind: SubjectKind::AgentRun as i32,
                    repo: repo_ref_id(&repo_id),
                    id: run.id.clone(),
                }),
                payload_json: serde_json::json!({
                    "product": "agent_tokens",
                    "quantity": (delta.input_tokens + delta.output_tokens).to_string(),
                    "unit": "tokens",
                    "sku": run.model,
                    "principal_id": rec.owner_id,
                    "repo_id": repo_id,
                    "key": format!("agent_run:{}", run.id),
                })
                .to_string(),
                ..Default::default()
            })
            .await?;
        }
        let terminal_now = terminal(run.state);
        if terminal_now {
            tx.delete(&RUN_TOKENS, id, "token").await?;
            tx.emit_event(run_event("agent_run.completed", &rec))
                .await?;
        } else {
            tx.emit_event(run_event("agent_run.state_changed", &rec))
                .await?;
        }
        tx.audit(run_audit("agent.run_finish", &rec)).await?;
        tx.commit().await?;
        self.run_done.notify_waiters();
        if !rec.watcher_id.is_empty() {
            self.charge_watcher(&repo_id, &rec.watcher_id, delta.usd_micros)
                .await?;
        }
        if !rec.feed_trigger.is_empty() {
            self.charge_feed(&rec.feed_trigger, delta.usd_micros)
                .await?;
        }
        // Link the CRs it opened to the issue (the work module closes it when they land).
        if rec.issue_number > 0 {
            if let Ok(repo) = self.forge.orgs.repo_any(&cx, &repo_id).await {
                let agent = run.agent.clone().unwrap_or_default();
                let acx =
                    Cx::new(agent.id.clone(), PrincipalKind::Agent).with_chain(rec.chain.clone());
                for n in &run.change_requests {
                    let Ok(num) = n.parse::<u64>() else { continue };
                    if let Ok(cr) = self.changes.get_by_number(&cx, &repo, num).await {
                        let _ = self
                            .forge
                            .work
                            .link_change(
                                &acx,
                                &RepoInfo::from(&repo),
                                &agent,
                                rec.issue_number,
                                ChangeLink {
                                    id: cr.id.clone(),
                                    repo_id: repo.id.clone(),
                                    number: cr.number,
                                    title: cr.title.clone(),
                                    state: "open".into(),
                                },
                                true,
                                IssueLinkSource::AgentRun,
                            )
                            .await;
                        self.link_session_cr(&repo.id, &run.session, &cr.id).await?;
                    }
                }
            }
        } else if let Ok(repo) = self.forge.orgs.repo_any(&cx, &repo_id).await {
            for n in &run.change_requests {
                if let Ok(num) = n.parse::<u64>()
                    && let Ok(cr) = self.changes.get_by_number(&cx, &repo, num).await
                {
                    self.link_session_cr(&repo.id, &run.session, &cr.id).await?;
                }
            }
        }
        if terminal_now {
            let reason = if run.failure_reason.is_empty() {
                super::registry::state_name(run.state)
            } else {
                run.failure_reason.to_lowercase()
            };
            self.after_finish(&rec, &reason).await;
        }
        Ok(())
    }

    /// Records that a session produced a change request (and the agent CR row).
    pub(crate) async fn link_session_cr(
        &self,
        repo_id: &str,
        session: &str,
        cr_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
        tx.put(
            &SESSION_LINKS,
            &Row::new(repo_id, format!("cr:{cr_id}")).k1(session),
        )
        .await?;
        if let Some((mut s, row)) = tx
            .get_msg::<SessionRec>(&SESSIONS, repo_id, session)
            .await?
            && !s.change_request_ids.iter().any(|c| c == cr_id)
        {
            s.change_request_ids.push(cr_id.to_string());
            tx.update(
                &SESSIONS,
                &Row::new(repo_id, session)
                    .k1(s.agent_id.clone())
                    .n1(s.created_ms as i64)
                    .version(row.version)
                    .msg(&s),
            )
            .await?;
            if let Some((mut c, crow)) = tx
                .get_msg::<super::model::AgentCrRec>(&AGENT_CRS, &s.agent_id, cr_id)
                .await?
                && c.session.is_empty()
            {
                c.session = session.to_string();
                tx.update(
                    &AGENT_CRS,
                    &Row::new(s.agent_id.clone(), cr_id)
                        .k1(c.repo_id.clone())
                        .n1(c.created_ms as i64)
                        .version(crow.version)
                        .msg(&c),
                )
                .await?;
            }
        }
        tx.commit().await
    }

    /// After a run ends: revoke its token, end its issue run.
    pub(crate) async fn after_finish(&self, rec: &RunRec, reason: &str) {
        if !rec.token_id.is_empty() {
            let _ = self.ports.issuer.revoke(&rec.token_id).await;
        }
        let run = rec.run();
        if rec.issue_number > 0
            && let Ok(repo) = self
                .forge
                .orgs
                .repo_any(&Cx::system(), &run_repo(&run))
                .await
        {
            let _ = self
                .forge
                .work
                .finish_agent_run(
                    &Cx::system(),
                    &RepoInfo::from(&repo),
                    rec.issue_number,
                    &run.id,
                    reason,
                )
                .await;
        }
        if !run.workspace_id.is_empty() {
            let _ = self
                .workspaces
                .stop_for_run(&Cx::system(), &run.workspace_id)
                .await;
        }
    }

    /// Moves a waiting run to its end state once all its change requests are finished.
    pub(crate) async fn settle_waiting(&self, run_id: &str) -> Result<()> {
        let cx = Cx::system();
        let rec = self.run_rec(&cx, run_id).await?;
        let run = rec.run();
        if run.state != St::WaitingForReview as i32 {
            return Ok(());
        }
        let repo = self.forge.orgs.repo_any(&cx, &run_repo(&run)).await?;
        let mut landed = 0;
        for n in &run.change_requests {
            let Ok(num) = n.parse::<u64>() else { continue };
            let cr = self.changes.get_by_number(&cx, &repo, num).await?;
            match bee_proto::v1::ChangeState::try_from(cr.state)
                .unwrap_or(bee_proto::v1::ChangeState::Unspecified)
            {
                bee_proto::v1::ChangeState::Landed => landed += 1,
                bee_proto::v1::ChangeState::Closed => {}
                _ => return Ok(()),
            }
        }
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        let (mut rec, row) = load_run(&mut tx, run_id).await?;
        if rec.run().state != St::WaitingForReview as i32 {
            return Ok(());
        }
        let (state, reason) = if landed > 0 {
            (St::Succeeded, "")
        } else {
            (St::Failed, "change_closed")
        };
        let mut r = rec.run();
        r.failure_reason = reason.to_string();
        rec.run = Some(r);
        set_state(
            &mut tx,
            &mut rec,
            state,
            if landed > 0 {
                "its change requests landed"
            } else {
                "its change requests were closed"
            },
        )
        .await?;
        save_run(&mut tx, &rec, &row).await?;
        tx.delete(&RUN_TOKENS, run_id, "token").await?;
        tx.emit_event(run_event("agent_run.completed", &rec))
            .await?;
        tx.audit(run_audit("agent.run_finish", &rec)).await?;
        tx.commit().await?;
        self.run_done.notify_waiters();
        self.after_finish(
            &rec,
            if landed > 0 {
                "change landed"
            } else {
                "change closed"
            },
        )
        .await;
        Ok(())
    }

    /// Runs whose session produced change request `cr_id`.
    pub(crate) async fn runs_of_cr(&self, repo_id: &str, cr_id: &str) -> Result<Vec<String>> {
        let s = self
            .sessions_for_change(&Cx::system(), repo_id, None, Some(cr_id))
            .await?;
        Ok(s.into_iter().map(|s| s.run_id).collect())
    }
}

/// Writes runner events into the run's session log as they arrive.
struct Reporter {
    agents: Arc<Agents>,
    run_id: String,
}

#[async_trait]
impl RunReporter for Reporter {
    async fn report(&self, event: RunnerEvent) -> Result<()> {
        let mut last = None;
        for _ in 0..5 {
            match self.try_report(&event).await {
                Ok(()) => {
                    self.agents.run_done.notify_waiters();
                    return Ok(());
                }
                Err(e) if e.reason == bee_proto::v1::ErrorReason::VersionMismatch => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| ForgeError::internal("report failed")))
    }
}

impl Reporter {
    async fn try_report(&self, event: &RunnerEvent) -> Result<()> {
        let a = &self.agents;
        let mut tx = a.db.begin(SURFACE, &Cx::system()).await?;
        let (mut rec, row) = load_run(&mut tx, &self.run_id).await?;
        if terminal(rec.run().state) {
            return Err(
                ForgeError::invalid_state("the run has finished").with_subcode("run_finished")
            );
        }
        let mut run = rec.run();
        let mut e = AgentRunEvent::default();
        let mut cancel = false;
        match event {
            RunnerEvent::Log(t) => {
                e.kind = Ek::Log as i32;
                e.text = t.clone();
            }
            RunnerEvent::Thought(t) => {
                e.kind = Ek::Thought as i32;
                e.text = t.clone();
            }
            RunnerEvent::ToolCall { name, payload } => {
                e.kind = Ek::ToolCall as i32;
                e.tool_name = name.clone();
                e.tool_payload = truncate(payload, 16 * 1024);
            }
            RunnerEvent::ToolResult { name, payload } => {
                e.kind = Ek::ToolResult as i32;
                e.tool_name = name.clone();
                e.tool_payload = truncate(payload, 16 * 1024);
            }
            RunnerEvent::ChangesetCreated(cs) => {
                e.kind = Ek::ChangesetCreated as i32;
                e.subject = Some(SubjectRef {
                    kind: SubjectKind::Changeset as i32,
                    repo: run.repo.clone(),
                    id: hex(&cs.raw),
                });
                if !run.changesets.contains(cs) {
                    run.changesets.push(cs.clone());
                }
                let budget = run.budget.unwrap_or_default();
                if budget.max_changesets > 0 && run.changesets.len() as u64 > budget.max_changesets
                {
                    rec.over_budget = true;
                    cancel = true;
                }
            }
            RunnerEvent::ChangeRequestOpened(n) => {
                e.kind = Ek::ChangeRequestOpened as i32;
                e.subject = Some(SubjectRef {
                    kind: SubjectKind::ChangeRequest as i32,
                    repo: run.repo.clone(),
                    id: n.to_string(),
                });
                let s = n.to_string();
                if !run.change_requests.contains(&s) {
                    run.change_requests.push(s);
                }
            }
            RunnerEvent::LeaseAcquired(id) => {
                e.kind = Ek::LeaseAcquired as i32;
                e.subject = Some(SubjectRef {
                    kind: SubjectKind::Repo as i32,
                    repo: run.repo.clone(),
                    id: id.clone(),
                });
                if !run.lease_ids.contains(id) {
                    run.lease_ids.push(id.clone());
                }
            }
            RunnerEvent::CheckResult { name, result } => {
                e.kind = Ek::CheckResult as i32;
                e.tool_name = name.clone();
                e.text = result.clone();
            }
            RunnerEvent::Cost(c) => {
                e.kind = Ek::CostUpdate as i32;
                let mut cur = run.cost.unwrap_or_default();
                if c.usd_micros >= cur.usd_micros
                    && c.input_tokens >= cur.input_tokens
                    && c.output_tokens >= cur.output_tokens
                {
                    cur = *c;
                } else {
                    add_cost(&mut cur, &Cost::default());
                }
                e.cost = Some(cur);
                run.cost = Some(cur);
                if over_budget(&run.budget.unwrap_or_default(), &cur).is_some() {
                    rec.over_budget = true;
                    cancel = true;
                }
            }
            RunnerEvent::Error(t) => {
                e.kind = Ek::Error as i32;
                e.text = t.clone();
            }
        }
        rec.run = Some(run);
        push_event(&mut tx, &mut rec, e).await?;
        if cancel {
            let mut w = ev(Ek::Error);
            w.text = "budget exceeded; stopping the run".into();
            push_event(&mut tx, &mut rec, w).await?;
        }
        save_run(&mut tx, &rec, &row).await?;
        tx.commit().await?;
        if cancel {
            let _ = a.ports.runner.cancel(&self.run_id).await;
        }
        Ok(())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopt_evaluate_needs_a_post() {
        let mut task = AgentTask {
            kind: AgentTaskKind::AdoptEvaluate as i32,
            ..Default::default()
        };
        let err = check_task(&task).unwrap_err();
        assert_eq!(err.subcode, "missing_field");
        task.post_id = "01J0000000000000000000POST".into();
        assert!(check_task(&task).is_ok());
    }
}
