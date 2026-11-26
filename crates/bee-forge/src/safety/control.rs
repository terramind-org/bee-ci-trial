//! Safety interrupts reuse runner cancellation. The organization flag is serialized with
//! run admission and with the final ref publication.

use bee_proto::v1::{
    Event, Org, PrincipalKind, Repo, SafetyStop, SafetyWorkKind, SubjectKind, SubjectRef,
};
use serde_json::json;

use crate::agents::model::RunRec;
use crate::agents::runs::terminal;
use crate::agents::{Agents, RUNS, SURFACE};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Cursor, Db, PublicationLock, Row, Scan, TableDef, Tx};

/// The policy flag lives with runs so their interruption commits with the stop.
pub(super) static LAND_FREEZE: TableDef = TableDef {
    name: "forge_safety_land_freeze",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

const FLAG: &str = "freeze";
const PAGE_SIZE: usize = 128;
const MAX_STOP_IDS: usize = 1024;
pub(crate) const FROZEN: &str = "safety_merge_frozen";

fn freeze_error() -> ForgeError {
    ForgeError::policy_denied(
        FROZEN,
        vec!["organization_resume".into()],
        "This organization has paused merges by agents and services",
    )
    .with_subcode(FROZEN)
}

async fn lock_flag(tx: &mut Tx, org: &str) -> Result<bool> {
    // The counter upsert locks even a previously absent row in Postgres. SQLite and the
    // memory backend already serialize the whole surface.
    Ok(tx.add_n1(&LAND_FREEZE, org, FLAG, 0).await? != 0)
}

pub(crate) async fn require_running_in_tx(tx: &mut Tx, org: &str) -> Result<()> {
    if lock_flag(tx, org).await? {
        return Err(
            ForgeError::invalid_state("This organization has stopped agent work")
                .with_subcode("safety_work_stopped"),
        );
    }
    Ok(())
}

pub(crate) async fn landing_frozen(db: &Db, cx: &Cx, org: &str) -> Result<bool> {
    if cx.actor_kind == PrincipalKind::Human {
        return Ok(false);
    }
    read_flag(db, cx, org).await
}

async fn read_flag(db: &Db, cx: &Cx, org: &str) -> Result<bool> {
    let mut tx = db.begin(SURFACE, cx).await?;
    let frozen = tx
        .get(&LAND_FREEZE, org, FLAG)
        .await?
        .and_then(|row| row.n1)
        .unwrap_or_default()
        != 0;
    tx.commit().await?;
    Ok(frozen)
}

pub(crate) async fn require_landing(db: &Db, cx: &Cx, org: &str) -> Result<()> {
    if landing_frozen(db, cx, org).await? {
        return Err(freeze_error());
    }
    Ok(())
}

/// A publication permit. Hold it until the native ref transaction has completed.
pub struct LandFence {
    _lock: PublicationLock,
}

/// Acquire a shared, backend-wide organization lock, then read the durable flag in a short
/// transaction. Stop and Resume take the exclusive lock before updating the flag. A Stop
/// that has returned has therefore drained earlier publishers, and later publishers see
/// the freeze. The permit holds no agents-store transaction, so unrelated writes proceed.
pub async fn fence_landing(db: &Db, cx: &Cx, org: &str) -> Result<LandFence> {
    let lock = db.store().publication_lock(org, true, cx.deadline).await?;
    if read_flag(db, cx, org).await? {
        return Err(freeze_error());
    }
    Ok(LandFence { _lock: lock })
}

/// Holds a publication permit for an authenticated agent or service.
pub async fn fence_nonhuman(db: &Db, cx: &Cx, org: &str) -> Result<Option<LandFence>> {
    if cx.actor_kind == PrincipalKind::Human {
        return Ok(None);
    }
    fence_landing(db, cx, org).await.map(Some)
}

/// Runner effects that must happen only after the cancellation transaction commits.
#[derive(Default)]
pub(crate) struct Interruption {
    cancelled: Vec<(RunRec, bool)>,
    paused_swarm: bool,
}

impl Interruption {
    pub(crate) fn stopped_run_ids(&self) -> Vec<String> {
        self.cancelled.iter().map(|(run, _)| run.run().id).collect()
    }

    pub(crate) fn paused_swarm(&self) -> bool {
        self.paused_swarm
    }

    async fn cancel(&mut self, agents: &Agents, tx: &mut Tx, id: &str, reason: &str) -> Result<()> {
        let cx = tx.cx().clone();
        if let Some(run) = agents.cancel_in_tx(&cx, tx, id, reason).await? {
            self.cancelled.push(run);
        }
        Ok(())
    }

    pub(crate) async fn finish(self, agents: &Agents) {
        for (run, was_running) in self.cancelled {
            agents.after_cancel(&run, was_running).await;
        }
    }
}

/// Pauses a swarm and interrupts its members, or stops the requested run. The caller
/// authorizes the operation, commits the attention item, then applies the runner effects.
pub(crate) async fn interrupt_in_tx(
    agents: &Agents,
    tx: &mut Tx,
    repo: &Repo,
    kind: SafetyWorkKind,
    id: &str,
    reason: &str,
) -> Result<Interruption> {
    let mut interruption = Interruption::default();
    match kind {
        SafetyWorkKind::Run => {
            if tx.get(&RUNS, &repo.id, id).await?.is_none() {
                return Err(ForgeError::not_found("Agent run").with_subcode("safety_run_missing"));
            }
            crate::swarms::store::interrupt_run_attempt(tx, repo, id, reason).await?;
            interruption.cancel(agents, tx, id, reason).await?;
        }
        SafetyWorkKind::Swarm => {
            let (paused, members) =
                crate::swarms::store::pause_for_safety(tx, repo, id, reason).await?;
            interruption.paused_swarm = paused;
            for member in members {
                interruption.cancel(agents, tx, &member, reason).await?;
            }
        }
        SafetyWorkKind::Unspecified => return Err(ForgeError::missing("work.kind")),
    }
    Ok(interruption)
}

async fn stop_repo(
    agents: &Agents,
    tx: &mut Tx,
    repo: &Repo,
    interrupted: &mut Interruption,
    swarms: &mut Vec<String>,
) -> Result<()> {
    const REASON: &str = "Organization safety stop";
    // Lock swarms before runs, matching join, dispatch and normal swarm stop.
    let mut cursor = None;
    loop {
        let rows = crate::swarms::store::safety_swarm_ids(tx, &repo.id, cursor).await?;
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(Cursor::after);
        for row in rows {
            if crate::swarms::store::pause_for_safety(tx, repo, &row.id, REASON)
                .await?
                .0
            {
                swarms.push(row.id);
            }
        }
    }

    let mut cursor = None;
    loop {
        let rows = tx
            .scan_msgs::<RunRec>(&RUNS, &Scan::scope(&repo.id, PAGE_SIZE).after(cursor))
            .await?;
        cursor = rows.last().map(|(_, row)| Cursor::after(row));
        let done = rows.len() < PAGE_SIZE;
        for (run, row) in rows {
            if !terminal(run.run().state) {
                interrupted.cancel(agents, tx, &row.id, REASON).await?;
            }
        }
        if done {
            break;
        }
    }
    Ok(())
}

fn stop_event(org: &Org, repo: Option<&Repo>, kind: &str) -> Event {
    Event {
        kind: kind.into(),
        org_id: org.id.clone(),
        repo_id: repo.map_or_else(String::new, |repo| repo.id.clone()),
        subject: Some(SubjectRef {
            kind: SubjectKind::Org as i32,
            id: org.id.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Applies an authorized organization stop or resume and records the action.
pub(crate) async fn set_stop(
    agents: &Agents,
    cx: &Cx,
    org: &Org,
    frozen: bool,
    reason: &str,
) -> Result<SafetyStop> {
    let publication = agents
        .db
        .store()
        .publication_lock(&org.id, false, cx.deadline)
        .await?;
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    lock_flag(&mut tx, &org.id).await?;
    tx.put(&LAND_FREEZE, &Row::new(&org.id, FLAG).n1(i64::from(frozen)))
        .await?;
    let kind = if frozen {
        "safety.stopped"
    } else {
        "safety.resumed"
    };
    let mut interrupted = Interruption::default();
    let mut swarms = Vec::new();
    let mut after = String::new();
    let mut deleted = false;
    loop {
        let orgs = &agents.forge.orgs;
        let (repos, next) = if deleted {
            orgs.list_deleted_repos(cx, &org.id, &after, PAGE_SIZE)
                .await?
        } else {
            orgs.list_repos(cx, &org.id, &after, PAGE_SIZE).await?
        };
        for repo in repos {
            if frozen {
                stop_repo(agents, &mut tx, &repo, &mut interrupted, &mut swarms).await?;
            }
            // Repository listeners wake pending merges on the affected refs on resume.
            if !deleted {
                tx.emit_event(stop_event(org, Some(&repo), kind)).await?;
            }
        }
        if next.is_empty() {
            // A recently deleted repository can still have an executing run.
            if frozen && !deleted {
                deleted = true;
                after.clear();
                continue;
            }
            break;
        }
        after = next;
    }
    tx.emit_event(stop_event(org, None, kind)).await?;
    let mut runs = interrupted.stopped_run_ids();
    runs.sort();
    swarms.sort();
    let result = SafetyStop {
        org: org.id.clone(),
        frozen,
        updated_at: Some(ts(tx.now_ms())),
        stopped_run_count: runs.len().min(u32::MAX as usize) as u32,
        paused_swarm_count: swarms.len().min(u32::MAX as usize) as u32,
        stopped_run_ids: runs.into_iter().take(MAX_STOP_IDS).collect(),
        paused_swarm_ids: swarms.into_iter().take(MAX_STOP_IDS).collect(),
    };
    tx.audit(
        AuditDraft::new(kind, &org.id)
            .target(SubjectKind::Org, &org.id)
            .after(
                &json!({
                    "reason": reason,
                    "frozen": frozen,
                    "stopped_run_count": result.stopped_run_count,
                    "paused_swarm_count": result.paused_swarm_count,
                })
                .to_string(),
            ),
    )
    .await?;
    tx.commit().await?;
    drop(publication);
    interrupted.finish(agents).await;
    if !frozen {
        agents.wake.notify_one();
    }
    Ok(result)
}
