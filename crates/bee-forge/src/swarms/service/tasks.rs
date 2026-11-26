//! A swarm task is a claim with its own lifecycle. An AgentTask is instead the prompt for
//! a run, which can make multiple attempts and coordinate multiple swarm tasks.

use super::*;
use crate::agents::model::RunRec;

const MAX_TASKS: usize = 200;
const MAX_ATTEMPTS: usize = 50;

async fn member(
    tx: &mut Tx,
    repo: &Repo,
    record: &Record,
    caller: &Caller,
    id: &str,
) -> Result<RunRec> {
    if !record.swarm.member_run_ids.iter().any(|run| run == id) {
        return Err(deny("The run must be a swarm member"));
    }
    let rec = store::run(tx, &repo.id, id, true).await?;
    let run = rec.run();
    if !is_lead(caller, record)
        && !is_owner(caller, &record.swarm)
        && !is_run(caller, run.agent.as_ref(), &rec.token_id)
    {
        return Err(deny(
            "Only this member run, the lead, or its owner can record this attempt",
        ));
    }
    Ok(rec)
}

fn task_mut<'a>(swarm: &'a mut Swarm, id: &str) -> Result<&'a mut SwarmTask> {
    swarm
        .tasks
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or_else(|| ForgeError::not_found("Swarm task").with_subcode("swarm_task_missing"))
}

fn hand_back(value: Option<SwarmHandBack>) -> Result<SwarmHandBack> {
    let value = value.ok_or_else(|| ForgeError::missing("hand_back"))?;
    for entries in [
        &value.done,
        &value.deviations,
        &value.concerns,
        &value.follow_ups,
    ] {
        if entries.len() > 32 {
            return Err(ForgeError::too_large("hand_back", 32, entries.len() as u64));
        }
        for entry in entries {
            model::line("hand_back entry", entry, 1000)?;
        }
    }
    let size = prost::Message::encoded_len(&value);
    if size > 8192 {
        return Err(ForgeError::too_large("hand_back", 8192, size as u64));
    }
    Ok(value)
}

impl SwarmApi {
    pub(super) async fn add_task(
        &self,
        caller: &Caller,
        mut req: AddSwarmTaskRequest,
    ) -> Result<SwarmTask> {
        model::line("title", &req.title, 240)?;
        model::paths(&mut req.paths)?;
        let (repo, _, mut tx, mut record, row) = self.mutate(caller, &req.swarm).await?;
        lead_or_owner(caller, &record)?;
        store::require_active(&record.swarm)?;
        if record.swarm.tasks.len() >= MAX_TASKS {
            return Err(ForgeError::too_large(
                "tasks",
                MAX_TASKS as u64,
                (MAX_TASKS + 1) as u64,
            ));
        }
        let task = SwarmTask {
            id: tx.new_id()?,
            title: req.title,
            paths: req.paths,
            state: SwarmTaskState::Open as i32,
            ..Default::default()
        };
        record.swarm.tasks.push(task.clone());
        store::save(
            &mut tx,
            &repo,
            &mut record,
            &row,
            "swarm.task_added",
            json!({"task_id": task.id}),
        )
        .await?;
        tx.commit().await?;
        Ok(task)
    }

    pub(super) async fn start_task(
        &self,
        caller: &Caller,
        req: StartSwarmTaskRequest,
    ) -> Result<SwarmTask> {
        let (repo, _, mut tx, mut record, row) = self.mutate(caller, &req.swarm).await?;
        let run = member(&mut tx, &repo, &record, caller, &req.run_id)
            .await?
            .run();
        if record.swarm.state() != SwarmState::Working || terminal(run.state) {
            return Err(invalid_state(
                "Starting an attempt needs a working swarm and an active run",
            ));
        }
        let active = record
            .swarm
            .tasks
            .iter()
            .filter(|t| t.state() == SwarmTaskState::Working);
        let claimed: Vec<_> = active
            .map(|t| (t.run_id.clone(), t.paths.clone()))
            .collect();
        let task = task_mut(&mut record.swarm, &req.task_id)?;
        if !matches!(task.state(), SwarmTaskState::Open | SwarmTaskState::Empty) {
            return Err(invalid_state("Only an open or empty task can be started"));
        }
        if task.attempts.len() >= MAX_ATTEMPTS {
            return Err(ForgeError::too_large(
                "attempts",
                MAX_ATTEMPTS as u64,
                (MAX_ATTEMPTS + 1) as u64,
            ));
        }
        if claimed
            .iter()
            .any(|(r, paths)| r == &run.id || model::paths_overlap(paths, &task.paths))
        {
            return Err(invalid_state("The run or a task path is already claimed")
                .with_subcode("swarm_task_claim"));
        }
        task.state = SwarmTaskState::Working as i32;
        task.run_id = run.id.clone();
        task.attempts.push(SwarmAttempt {
            run_id: run.id,
            agent: run.agent.map(public_principal),
            started_at: Some(ts(tx.now_ms())),
            ..Default::default()
        });
        let task = task.clone();
        store::save(
            &mut tx,
            &repo,
            &mut record,
            &row,
            "swarm.task_started",
            json!({"task_id": task.id, "run_id": task.run_id}),
        )
        .await?;
        tx.commit().await?;
        Ok(task)
    }

    pub(super) async fn finish_task(
        &self,
        caller: &Caller,
        req: FinishSwarmTaskRequest,
    ) -> Result<SwarmTask> {
        let outcome = req.outcome();
        if outcome == SwarmAttemptOutcome::Unspecified {
            return Err(ForgeError::invalid(
                "swarm_outcome",
                "Select an attempt outcome",
            ));
        }
        model::line("reason", &req.reason, 500)?;
        let hand_back = hand_back(req.hand_back)?;

        let (repo, _, mut tx, mut record, row) = self.mutate(caller, &req.swarm).await?;
        store::require_active(&record.swarm)?;
        member(&mut tx, &repo, &record, caller, &req.run_id).await?;
        let task = task_mut(&mut record.swarm, &req.task_id)?;
        if req.attempt_number == 0 {
            return Err(ForgeError::missing("attempt_number"));
        }
        if req.attempt_number as usize != task.attempts.len() {
            return Err(ForgeError::version_mismatch(
                u64::from(req.attempt_number),
                task.attempts.len() as u64,
            )
            .with_subcode("swarm_attempt_changed"));
        }
        if task.state() != SwarmTaskState::Working || task.run_id != req.run_id {
            return Err(invalid_state("There is no active attempt by this run"));
        }
        end(task, outcome, req.reason, hand_back, ts(tx.now_ms()))?;
        let task = task.clone();
        store::save(
            &mut tx,
            &repo,
            &mut record,
            &row,
            "swarm.task_finished",
            json!({"task_id": task.id, "run_id": req.run_id, "outcome": outcome.as_str_name()}),
        )
        .await?;
        tx.commit().await?;
        Ok(task)
    }
}

fn end(
    task: &mut SwarmTask,
    outcome: SwarmAttemptOutcome,
    reason: String,
    hand_back: SwarmHandBack,
    now: pbjson_types::Timestamp,
) -> Result<()> {
    let attempt = task
        .attempts
        .last_mut()
        .ok_or_else(|| ForgeError::internal("Missing task attempt"))?;
    attempt.finished_at = Some(now);
    attempt.outcome = outcome as i32;
    attempt.reason = reason;
    attempt.hand_back = Some(hand_back);
    model::end_attempt(task, outcome);
    Ok(())
}

pub(in crate::swarms) async fn interrupt_attempts(
    tx: &mut Tx,
    repo: &Repo,
    record: &mut Record,
    run: Option<&str>,
    reason: &str,
) -> Result<bool> {
    let mut events = Vec::new();
    let now = ts(tx.now_ms());
    for task in &mut record.swarm.tasks {
        if task.state() == SwarmTaskState::Working && run.is_none_or(|run| task.run_id == run) {
            let run_id = task.run_id.clone();
            end(
                task,
                SwarmAttemptOutcome::Empty,
                reason.into(),
                SwarmHandBack::default(),
                now,
            )?;
            events.push(json!({"task_id": task.id, "run_id": run_id, "outcome": "SWARM_ATTEMPT_OUTCOME_EMPTY"}));
        }
    }
    let changed = !events.is_empty();
    for event in events {
        store::emit(
            tx,
            &repo.id,
            &repo.org_id,
            &record.swarm,
            "swarm.task_finished",
            event,
        )
        .await?;
    }
    Ok(changed)
}
