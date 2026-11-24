//! Assignees, including agent principals. Assigning an agent that takes work starts a run:
//! the issue records the run id and an `issue.assigned_to_agent` event tells the agent runner.

use bee_proto::v1::{
    Event, Issue, IssueTimelineEventKind as Tk, Principal, PrincipalKind, SubjectKind,
};

use super::issues::{Cache, add_timeline, load_issue, render, save_issue};
use super::model::IssueRec;
use super::*;

/// An agent run to start because the agent was assigned.
#[derive(Clone, Debug, Default)]
pub struct AgentAssignment {
    /// The agent.
    pub agent: Principal,
    /// Extra instructions for the run.
    pub instructions: String,
    /// Ref the run should branch from (empty = default ref).
    pub base_ref: String,
}

/// Whether `p` is an agent principal.
pub fn is_agent(p: &Principal) -> bool {
    p.kind == PrincipalKind::Agent as i32
}

/// Starts runs: records them on the issue and emits `issue.assigned_to_agent` for each.
pub(crate) async fn start_runs(
    tx: &mut Tx,
    repo: &RepoInfo,
    rec: &mut IssueRec,
    actor: &Principal,
    assigns: &[AgentAssignment],
) -> Result<Vec<(Principal, String)>> {
    let mut out = Vec::new();
    for a in assigns {
        if !rec.assignees.iter().any(|x| x.id == a.agent.id) {
            continue;
        }
        let run_id = tx.new_id()?;
        rec.active_agent_runs
            .push(format!("{}/{run_id}", a.agent.id));
        let rid = run_id.clone();
        let agent = a.agent.clone();
        add_timeline(tx, &rec.id, Tk::AgentRunStarted, actor, |e| {
            e.agent_run_id = rid;
            e.assignee = Some(agent);
        })
        .await?;
        let payload = serde_json::json!({
            "repo_id": repo.id,
            "repo_full_name": repo.full_name,
            "issue_id": rec.id,
            "number": rec.number,
            "title": rec.title,
            "body": rec.body,
            "agent": serde_json::from_str::<serde_json::Value>(&pjson!(&a.agent)).unwrap_or_default(),
            "agent_run_id": run_id,
            "instructions": a.instructions,
            "base_ref": a.base_ref,
            "assigned_by": actor.id,
        })
        .to_string();
        tx.emit_event(Event {
            kind: "issue.assigned_to_agent".into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            subject: Some(subject(SubjectKind::Issue, &repo.id, &rec.id)),
            payload_json: payload,
            ..Default::default()
        })
        .await?;
        out.push((a.agent.clone(), run_id));
    }
    Ok(out)
}

/// Ends the runs of `agent_id` on `rec` (all of them when `agent_id` is empty, or just the
/// run `only`), with a timeline entry and an `issue.agent_run_finished` event each.
pub(crate) async fn finish_runs(
    tx: &mut Tx,
    repo: &RepoInfo,
    rec: &mut IssueRec,
    actor: &Principal,
    agent_id: &str,
    only: &str,
    reason: &str,
) -> Result<usize> {
    let (done, keep): (Vec<String>, Vec<String>) =
        rec.active_agent_runs.iter().cloned().partition(|r| {
            let (a, id) = r.split_once('/').unwrap_or(("", r));
            (agent_id.is_empty() || a == agent_id) && (only.is_empty() || id == only)
        });
    rec.active_agent_runs = keep;
    for r in &done {
        let (a, id) = r.split_once('/').unwrap_or(("", r));
        let rid = id.to_string();
        let agent = Principal {
            id: a.to_string(),
            ..Default::default()
        };
        add_timeline(tx, &rec.id, Tk::AgentRunFinished, actor, |e| {
            e.agent_run_id = rid;
            e.assignee = Some(agent);
            e.current_value = reason.to_string();
        })
        .await?;
        tx.emit_event(Event {
            kind: "issue.agent_run_finished".into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            subject: Some(subject(SubjectKind::Issue, &repo.id, &rec.id)),
            payload_json: serde_json::json!({
                "issue_id": rec.id, "number": rec.number, "agent_id": a,
                "agent_run_id": id, "reason": reason,
            })
            .to_string(),
            ..Default::default()
        })
        .await?;
    }
    Ok(done.len())
}

impl Work {
    /// Assigns principals. Agents in `runs` (the ones the service found willing and funded)
    /// start a run each; the run ids are returned.
    pub async fn add_assignees(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        principals: &[Principal],
        runs: &[AgentAssignment],
    ) -> Result<(Issue, Vec<(Principal, String)>)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let old = rec.clone();
        let mut added: Vec<Principal> = Vec::new();
        for p in principals {
            if !rec.assignees.iter().any(|x| x.id == p.id) && !added.iter().any(|x| x.id == p.id) {
                added.push(p.clone());
            }
        }
        let total = rec.assignees.len() + added.len();
        if total > limits::ASSIGNEES {
            return Err(exhausted("assignees", limits::ASSIGNEES, total));
        }
        rec.assignees.extend(added.iter().cloned());
        for p in &added {
            let p = p.clone();
            add_timeline(&mut tx, &rec.id, Tk::Assigned, &actor.principal, |e| {
                e.assignee = Some(p)
            })
            .await?;
        }
        // Only newly assigned agents start a run; re-assigning one does nothing.
        let fresh: Vec<AgentAssignment> = runs
            .iter()
            .filter(|r| added.iter().any(|a| a.id == r.agent.id))
            .cloned()
            .collect();
        let started = start_runs(&mut tx, repo, &mut rec, &actor.principal, &fresh).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        if added.is_empty() {
            return Ok((
                render(&mut tx, &mut cache, &rec, row.version).await?,
                started,
            ));
        }
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            Change {
                kind: "issue.assigned",
                action: "issue.assign",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id: &rec.id,
                payload: serde_json::json!({
                    "issue": serde_json::from_str::<serde_json::Value>(&pjson!(&issue)).unwrap_or_default(),
                    "assignees": added.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                })
                .to_string(),
                after: serde_json::json!({
                    "number": rec.number,
                    "assignees": added.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                    "agent_runs": started.iter().map(|(a, r)| serde_json::json!({"agent": a.id, "run": r})).collect::<Vec<_>>(),
                })
                .to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok((issue, started))
    }

    /// Unassigns principals; with `stop_agent_runs` the runs of removed agents end too.
    pub async fn remove_assignees(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        principal_ids: &[String],
        stop_agent_runs: bool,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let old = rec.clone();
        let removed: Vec<Principal> = rec
            .assignees
            .iter()
            .filter(|a| principal_ids.contains(&a.id))
            .cloned()
            .collect();
        let mut cache = Cache::new(repo, &cx.actor_id);
        if removed.is_empty() {
            return render(&mut tx, &mut cache, &rec, row.version).await;
        }
        rec.assignees.retain(|a| !principal_ids.contains(&a.id));
        for p in &removed {
            let p2 = p.clone();
            add_timeline(&mut tx, &rec.id, Tk::Unassigned, &actor.principal, |e| {
                e.assignee = Some(p2)
            })
            .await?;
            if stop_agent_runs && is_agent(p) {
                finish_runs(
                    &mut tx,
                    repo,
                    &mut rec,
                    &actor.principal,
                    &p.id,
                    "",
                    "stopped",
                )
                .await?;
            }
        }
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            Change {
                kind: "issue.unassigned",
                action: "issue.unassign",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id: &rec.id,
                payload: serde_json::json!({
                    "issue": serde_json::from_str::<serde_json::Value>(&pjson!(&issue)).unwrap_or_default(),
                    "assignees": removed.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                })
                .to_string(),
                after: serde_json::json!({
                    "number": rec.number,
                    "assignees": removed.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                    "stopped_runs": stop_agent_runs,
                })
                .to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// The agent runner reports that run `run_id` on an issue ended.
    pub async fn finish_agent_run(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
        run_id: &str,
        reason: &str,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let old = rec.clone();
        let who = cx.principal();
        let n = finish_runs(&mut tx, repo, &mut rec, &who, "", run_id, reason).await?;
        if n == 0 {
            return Err(ForgeError::not_found(format!("agent run {run_id}")));
        }
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        tx.audit(
            crate::audit::AuditDraft::new("issue.finish_agent_run", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Issue, &rec.id)
                .after(
                    &serde_json::json!({"number": number, "run": run_id, "reason": reason})
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }
}
