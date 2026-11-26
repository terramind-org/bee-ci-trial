use std::collections::BTreeSet;

use bee_proto::v1::*;
use prost::Message as _;
use serde_json::json;

use super::model::{Record, add_budget, invalid_state};
use super::{MEMBERS, SWARMS};
use crate::agents::model::{RunRec, SessionRec};
use crate::agents::registry::{add_cost, repo_ref_id};
use crate::agents::{Agents, RUN_EVENTS, RUNS, SESSIONS, SURFACE, pad};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::store::{Cursor, Row, Scan, Tx};

// Keep encoded responses within the transport limit.
const MAX_RECORD_BYTES: usize = 512 * 1024;
const MAX_REPLY_BYTES: usize = 3 * 1024 * 1024;

pub(super) async fn load(tx: &mut Tx, repo: &str, id: &str, lock: bool) -> Result<(Record, Row)> {
    if id.is_empty() {
        return Err(ForgeError::missing("swarm.id"));
    }
    let row = if lock {
        tx.get_for_update(&SWARMS, repo, id).await?
    } else {
        tx.get(&SWARMS, repo, id).await?
    }
    .ok_or_else(|| ForgeError::not_found(format!("Swarm {id}")).with_subcode("swarm_missing"))?;
    Ok((row.decode()?, row))
}

pub(super) async fn save(
    tx: &mut Tx,
    repo: &Repo,
    record: &mut Record,
    row: &Row,
    kind: &str,
    details: serde_json::Value,
) -> Result<()> {
    // Reserve space at the start of each attempt and offer, so they can always end.
    // A hand-back is capped at 8192 bytes and its one-line reason at 500 characters.
    let attempts = record
        .swarm
        .tasks
        .iter()
        .filter(|t| t.state() == SwarmTaskState::Working)
        .count();
    let pending = record
        .swarm
        .joins
        .iter()
        .filter(|j| j.state() == SwarmJoinState::Pending)
        .count();
    let size = record.encoded_len() + attempts * 10_300 + pending * 2_100;
    if size > MAX_RECORD_BYTES {
        return Err(ForgeError::too_large(
            "swarm",
            MAX_RECORD_BYTES as u64,
            size as u64,
        ));
    }
    record.swarm.updated_at = Some(ts(tx.now_ms()));
    tx.update(&SWARMS, &row.clone().msg(record)).await?;
    emit(tx, &repo.id, &repo.org_id, &record.swarm, kind, details).await
}

pub(super) async fn emit(
    tx: &mut Tx,
    repo: &str,
    org: &str,
    swarm: &Swarm,
    kind: &str,
    details: serde_json::Value,
) -> Result<()> {
    tx.emit_event(Event {
        kind: kind.into(),
        repo_id: repo.into(),
        org_id: org.into(),
        subject: Some(SubjectRef {
            kind: SubjectKind::Swarm as i32,
            repo: repo_ref_id(repo),
            id: swarm.id.clone(),
        }),
        payload_json: details.to_string(),
        ..Default::default()
    })
    .await?;
    tx.audit(
        AuditDraft::new(kind, org)
            .repo_id(repo)
            .target(SubjectKind::Swarm, &swarm.id)
            .after(&details.to_string()),
    )
    .await
}

pub(super) async fn insert(tx: &mut Tx, repo: &Repo, record: &Record) -> Result<()> {
    let s = &record.swarm;
    insert_member(tx, &repo.id, &s.id, &s.lead_run_id).await?;
    tx.insert(&SWARMS, &Row::new(&repo.id, &s.id).msg(record))
        .await?;
    emit(tx, &repo.id, &repo.org_id, s, "swarm.created", json!({})).await
}

async fn insert_member(tx: &mut Tx, repo: &str, swarm: &str, run: &str) -> Result<()> {
    tx.insert(&MEMBERS, &Row::new(repo, run).k1(swarm))
        .await
        .map_err(|e| {
            if e.reason == ErrorReason::AlreadyExists {
                e.with_subcode("swarm_member")
            } else {
                e
            }
        })
}

pub(super) async fn run(tx: &mut Tx, repo: &str, id: &str, lock: bool) -> Result<RunRec> {
    let row = if lock {
        tx.get_for_update(&RUNS, repo, id).await?
    } else {
        tx.get(&RUNS, repo, id).await?
    }
    .ok_or_else(|| {
        ForgeError::not_found(format!("Agent run {id}")).with_subcode("swarm_run_missing")
    })?;
    row.decode()
}

pub(super) fn require_active(swarm: &Swarm) -> Result<()> {
    match swarm.state() {
        SwarmState::Planning | SwarmState::Working | SwarmState::Paused => Ok(()),
        _ => Err(invalid_state("Swarm has ended")),
    }
}

pub(super) fn pending_join<'a>(record: &'a mut Record, join_id: &str) -> Result<&'a mut SwarmJoin> {
    let join = record
        .swarm
        .joins
        .iter_mut()
        .find(|join| join.id == join_id)
        .ok_or_else(|| ForgeError::not_found("Join request").with_subcode("swarm_join_missing"))?;
    if join.state() != SwarmJoinState::Pending {
        return Err(invalid_state("Join request has already been decided"));
    }
    Ok(join)
}

/// Starts the run and admits it atomically. The caller must take the swarm lock before
/// creating the run or minting its token, so two decisions cannot both start a worker.
pub(crate) async fn admit(
    tx: &mut Tx,
    repo: &Repo,
    reference: &str,
    reason: &str,
    run: &AgentRun,
) -> Result<()> {
    let (swarm_id, join_id) = reference
        .split_once('/')
        .ok_or_else(|| ForgeError::internal("Invalid swarm join reference"))?;
    let (mut record, row) = load(tx, &repo.id, swarm_id, true).await?;
    require_active(&record.swarm)?;
    let now = tx.now_ms();
    let join = pending_join(&mut record, join_id)?;
    if join.agent.as_ref().map(|a| &a.id) != run.agent.as_ref().map(|a| &a.id) {
        return Err(ForgeError::internal("Swarm join agent changed"));
    }
    join.state = SwarmJoinState::Accepted as i32;
    join.run_id.clone_from(&run.id);
    join.reason = reason.into();
    join.decided_at = Some(ts(now));
    record.swarm.member_run_ids.push(run.id.clone());
    insert_member(tx, &repo.id, swarm_id, &run.id).await?;
    save(
        tx,
        repo,
        &mut record,
        &row,
        "swarm.join_decided",
        json!({"join_id": join_id, "state": "accepted", "run_id": run.id}),
    )
    .await
}

/// Takes the same swarm lock used by pause and stop before dispatching a member run.
pub(crate) async fn may_dispatch(tx: &mut Tx, repo: &str, run: &str) -> Result<bool> {
    let Some(member) = tx.get(&MEMBERS, repo, run).await? else {
        return Ok(true);
    };
    let swarm_id = member.k1.as_deref().unwrap_or_default();
    let (record, _) = load(tx, repo, swarm_id, true).await?;
    Ok(record.swarm.state() == SwarmState::Working
        || (record.swarm.state() == SwarmState::Planning && record.swarm.lead_run_id == run))
}

/// Terminal run transitions take the swarm lock before the run lock, like StartSwarmTask.
pub(crate) async fn lock_run_swarm(tx: &mut Tx, repo: &str, run: &str) -> Result<Option<String>> {
    let swarm = swarm_for_run(tx, repo, run).await?;
    if let Some(id) = &swarm {
        load(tx, repo, id, true).await?;
    }
    Ok(swarm)
}

pub(crate) async fn finish_run_attempt(
    tx: &mut Tx,
    repo: &str,
    org: &str,
    swarm: Option<&str>,
    run: &str,
    reason: &str,
) -> Result<()> {
    let Some(id) = swarm else { return Ok(()) };
    let (mut record, row) = load(tx, repo, id, false).await?;
    if let Some(details) =
        super::service::tasks::finish_run_attempt(&mut record, run, reason, ts(tx.now_ms()))?
    {
        record.swarm.updated_at = Some(ts(tx.now_ms()));
        tx.update(&SWARMS, &row.msg(&record)).await?;
        emit(tx, repo, org, &record.swarm, "swarm.task_finished", details).await?;
    }
    Ok(())
}

// Safety stop also covers planning swarms, whose lead can still run before work begins.
pub(crate) async fn pause_for_safety(
    tx: &mut Tx,
    repo: &Repo,
    id: &str,
    reason: &str,
) -> Result<(bool, Vec<String>)> {
    let (mut record, row) = load(tx, &repo.id, id, true).await?;
    let paused = match record.swarm.state() {
        SwarmState::Planning | SwarmState::Working => true,
        SwarmState::Paused => false,
        _ => return Ok((false, Vec::new())),
    };
    let ended =
        super::service::tasks::interrupt_attempts(tx, repo, &mut record, None, reason).await?;
    if paused || ended {
        record.swarm.state = SwarmState::Paused as i32;
        save(
            tx,
            repo,
            &mut record,
            &row,
            "swarm.state_changed",
            json!({"state": SwarmState::Paused.as_str_name(), "reason": reason}),
        )
        .await?;
    }
    Ok((paused, record.swarm.member_run_ids))
}

// A run interruption ends its active attempt, leaving the rest of its swarm able to work.
pub(crate) async fn interrupt_run_attempt(
    tx: &mut Tx,
    repo: &Repo,
    run: &str,
    reason: &str,
) -> Result<()> {
    let Some(member) = tx.get(&MEMBERS, &repo.id, run).await? else {
        return Ok(());
    };
    let id = member.k1.as_deref().unwrap_or_default();
    let (mut record, row) = load(tx, &repo.id, id, true).await?;
    if super::service::tasks::interrupt_attempts(tx, repo, &mut record, Some(run), reason).await? {
        record.swarm.updated_at = Some(ts(tx.now_ms()));
        tx.update(&SWARMS, &row.msg(&record)).await?;
    }
    Ok(())
}

pub(crate) async fn safety_swarm_ids(
    tx: &mut Tx,
    repo: &str,
    after: Option<Cursor>,
) -> Result<Vec<Row>> {
    tx.scan(&SWARMS, &Scan::scope(repo, 128).after(after)).await
}

/// Direct run steering still respects the swarm's person-to-lead boundary.
pub(crate) async fn guard_message(tx: &mut Tx, repo: &str, run: &str) -> Result<()> {
    let Some(member) = tx.get(&MEMBERS, repo, run).await? else {
        return Ok(());
    };
    let id = member.k1.as_deref().unwrap_or_default();
    let (record, _) = load(tx, repo, id, true).await?;
    if record.swarm.lead_run_id != run || tx.cx().actor_kind != PrincipalKind::Human {
        return Err(
            ForgeError::permission_denied("Only a person may message the lead of a swarm")
                .with_subcode("swarm_message_lead"),
        );
    }
    require_active(&record.swarm)
}

struct RunView {
    run: AgentRun,
    activity: u64,
    outcomes: Vec<String>,
}

async fn activity(tx: &mut Tx, run: &AgentRun) -> Result<u64> {
    let mut activity = [&run.created_at, &run.started_at, &run.finished_at]
        .into_iter()
        .filter_map(|t| t.as_ref().map(ts_ms))
        .max()
        .unwrap_or(0);
    if run.last_event_seq != 0
        && let Some((event, _)) = tx
            .get_msg::<AgentRunEvent>(&RUN_EVENTS, &run.id, &pad(run.last_event_seq))
            .await?
        && let Some(time) = event.time.as_ref()
    {
        activity = activity.max(ts_ms(time));
    }
    Ok(activity)
}

async fn run_view(tx: &mut Tx, repo: &str, rec: RunRec) -> Result<RunView> {
    let run = rec.run();
    let activity = activity(tx, &run).await?;
    let outcomes = if crate::agents::runs::terminal(run.state) {
        tx.get_msg::<SessionRec>(&SESSIONS, repo, &run.session)
            .await?
            .map(|(s, _)| s.change_request_ids)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(RunView {
        run,
        activity,
        outcomes,
    })
}

async fn fill_outcomes(agents: &Agents, cx: &Cx, repo: &Repo, view: &mut RunView) -> Result<()> {
    // Older sessions (or a just-finished run whose session links are being updated)
    // still have the runner's durable pull request numbers.
    if crate::agents::runs::terminal(view.run.state) {
        view.outcomes
            .extend(super::outcomes::changes(&agents.db, cx, &repo.id, &view.run.id).await?);
        for number in &view.run.change_requests {
            let Ok(number) = number.parse() else { continue };
            match agents.changes.get_by_number(cx, repo, number).await {
                Ok(change) => view.outcomes.push(change.id),
                Err(e) if e.reason == ErrorReason::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    view.outcomes.sort();
    view.outcomes.dedup();
    Ok(())
}

pub(super) async fn hydrate(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    mut swarm: Swarm,
) -> Result<Swarm> {
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    let mut members = Vec::with_capacity(swarm.member_run_ids.len());
    for id in &swarm.member_run_ids {
        let rec = run(&mut tx, &repo.id, id, false).await?;
        members.push(run_view(&mut tx, &repo.id, rec).await?);
    }
    drop(tx);
    let mut cost = Cost::default();
    let mut budget = None;
    let mut outcomes = BTreeSet::new();
    let mut updated = swarm.updated_at.as_ref().map(ts_ms).unwrap_or(0);
    for mut member in members {
        fill_outcomes(agents, cx, repo, &mut member).await?;
        add_cost(&mut cost, &member.run.cost.unwrap_or_default());
        let b = member.run.budget.unwrap_or_default();
        budget = Some(match budget {
            None => b,
            Some(previous) => add_budget(previous, b),
        });
        outcomes.extend(member.outcomes);
        updated = updated.max(member.activity);
    }
    swarm.cost = Some(cost);
    swarm.budget = budget;
    swarm.change_request_ids = outcomes.into_iter().collect();
    swarm.updated_at = Some(ts(updated));
    Ok(swarm)
}

fn page_cursor(after: &str) -> Result<Option<(u64, &str)>> {
    if after.is_empty() {
        return Ok(None);
    }
    after
        .split_once('/')
        .and_then(|(time, id)| Some((time.parse().ok()?, id)))
        .filter(|(_, id)| !id.is_empty() && !id.contains('/'))
        .map(Some)
        .ok_or_else(|| ForgeError::invalid("swarm_page", "Invalid repo work page token"))
}

/// For historical rows use keyset scans, and retain only the best page by activity.
pub(crate) async fn list(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    after: &str,
    size: usize,
) -> Result<(Vec<RepoWorkItem>, String)> {
    let after = page_cursor(after)?;
    let mut work = std::collections::BTreeMap::new();
    for table in [&SWARMS, &RUNS] {
        let mut cursor = None;
        loop {
            let mut tx = agents.db.begin(SURFACE, cx).await?;
            let rows = tx
                .scan(table, &Scan::scope(&repo.id, 128).after(cursor))
                .await?;
            cursor = rows.last().map(Cursor::after);
            let done = rows.len() < 128;
            for row in rows {
                let view = if table.name == SWARMS.name {
                    let record: Record = row.decode()?;
                    let mut last = record.swarm.updated_at.as_ref().map(ts_ms).unwrap_or(0);
                    for id in &record.swarm.member_run_ids {
                        let rec = run(&mut tx, &repo.id, id, false).await?;
                        last = last.max(activity(&mut tx, &rec.run()).await?);
                    }
                    (last, repo_work_item::Work::Swarm(record.swarm))
                } else {
                    if tx.get(&MEMBERS, &repo.id, &row.id).await?.is_some() {
                        continue;
                    }
                    let record: RunRec = row.decode()?;
                    let run = record.run();
                    (
                        activity(&mut tx, &run).await?,
                        repo_work_item::Work::Run(run),
                    )
                };
                let key = (view.0, row.id);
                if after.is_none_or(|after| (key.0, key.1.as_str()) < after) {
                    work.insert(key, view.1);
                    if work.len() > size + 1 {
                        work.pop_first();
                    }
                }
            }
            if done {
                break;
            }
        }
    }
    let total = work.len();
    let mut items = Vec::with_capacity(size);
    let mut token = String::new();
    let mut bytes = 0;
    for ((activity, id), entry) in work.into_iter().rev().take(size) {
        let (work, change_request_ids) = match entry {
            repo_work_item::Work::Swarm(s) => {
                let s = hydrate(agents, cx, repo, s).await?;
                let outcomes = s.change_request_ids.clone();
                (repo_work_item::Work::Swarm(s), outcomes)
            }
            repo_work_item::Work::Run(run) => {
                let mut tx = agents.db.begin(SURFACE, cx).await?;
                let rec = self::run(&mut tx, &repo.id, &run.id, false).await?;
                let mut view = run_view(&mut tx, &repo.id, rec).await?;
                drop(tx);
                fill_outcomes(agents, cx, repo, &mut view).await?;
                (repo_work_item::Work::Run(view.run), view.outcomes)
            }
        };
        let item = RepoWorkItem {
            work: Some(work),
            activity_at: Some(ts(activity)),
            change_request_ids,
        };
        bytes += item.encoded_len() + 8;
        if bytes > MAX_REPLY_BYTES {
            if items.is_empty() {
                return Err(ForgeError::too_large(
                    "work_item",
                    MAX_REPLY_BYTES as u64,
                    bytes as u64,
                ));
            }
            break;
        }
        items.push(item);
        token = format!("{activity}/{id}");
    }
    if items.len() == total {
        token.clear();
    }
    Ok((items, token))
}

/// Reads the persisted swarm for its asked-against-done record.
pub(crate) async fn get_swarm(tx: &mut Tx, repo: &str, id: &str) -> Result<Swarm> {
    Ok(load(tx, repo, id, false).await?.0.swarm)
}

/// Identifies the unit of work that answers for a member run.
pub(crate) async fn swarm_for_run(tx: &mut Tx, repo: &str, run: &str) -> Result<Option<String>> {
    Ok(tx.get(&MEMBERS, repo, run).await?.and_then(|row| row.k1))
}
