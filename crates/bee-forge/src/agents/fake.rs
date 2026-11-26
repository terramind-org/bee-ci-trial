//! [`FakeRunner`]: a deterministic [`AgentRunner`] for tests and `bee-server --dev`.
//!
//! It does what a coding agent does, through the forge's own APIs and without a model:
//!
//! - **issue, free-form, security-fix and revise tasks:** tries to take an exclusive lease on
//!   each candidate path in turn (routing around paths another agent holds), commits one file
//!   under the leased path on the base ref, and (issue tasks) opens a change request that
//!   links the issue; revise tasks add a revision to the change request.
//! - **review tasks:** submits a review with the scripted verdict (default: approve).
//! - **adopt-evaluate tasks:** opens an issue inspired by the post (`inspired_by`), titled
//!   after the post's change, or finishes "not relevant" when scripted to; the summary takes
//!   the form the feed trigger defines ([`super::feed::Outcome`]).
//!
//! Cost is a pure function of the prompt and the work done. Scripts per agent change the
//! behavior (fail, cost, candidate paths, verdict, hold until released).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_proto::v1::{AgentTaskKind, Cost, ErrorReason, LeaseMode, PrincipalKind, Verdict};
use tokio::sync::Notify;

use super::feed::Outcome;
use super::ports::{
    AgentRepo, AgentRunner, RunJob, RunOutcome, RunReporter, RunStatus, RunnerEvent,
};
use crate::changes::{Changes, NewChange};
use crate::cx::Cx;
use crate::error::Result;
use crate::leases::{AcquireRequest, Leases};
use crate::work::{Actor, NewIssue, RepoInfo};

/// How the fake runner behaves for one agent.
#[derive(Clone, Debug, Default)]
pub struct FakeScript {
    /// Fail with this reason instead of working.
    pub fail: Option<String>,
    /// Report this cost instead of the computed one.
    pub cost: Option<Cost>,
    /// Candidate paths to lease and edit, in order (default `agent/<run id>.md`).
    pub paths: Vec<String>,
    /// Review verdict (default approve).
    pub verdict: Option<Verdict>,
    /// Wait for [`FakeRunner::release`] before working.
    pub hold: bool,
    /// Do not open a change request for issue tasks.
    pub no_change_request: bool,
    /// Adopt-evaluate tasks: the post does not apply, for this reason.
    pub not_relevant: Option<String>,
}

/// The deterministic runner.
#[derive(Default)]
pub struct FakeRunner {
    changes: Option<Arc<Changes>>,
    repo: Option<Arc<dyn AgentRepo>>,
    leases: Option<Arc<Leases>>,
    scripts: Mutex<BTreeMap<String, FakeScript>>,
    jobs: Mutex<Vec<RunJob>>,
    steering: Mutex<Vec<(String, String)>>,
    cancelled: Mutex<Vec<String>>,
    gates: Mutex<BTreeMap<String, Arc<Notify>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl FakeRunner {
    /// A runner that only reports (no repo writes).
    pub fn new() -> FakeRunner {
        FakeRunner::default()
    }

    /// A runner that commits, leases and opens change requests through these modules.
    pub fn with_forge(
        changes: Arc<Changes>,
        repo: Arc<dyn AgentRepo>,
        leases: Arc<Leases>,
    ) -> FakeRunner {
        FakeRunner {
            changes: Some(changes),
            repo: Some(repo),
            leases: Some(leases),
            ..FakeRunner::default()
        }
    }

    /// Sets the script of an agent (`"*"` = every agent without its own).
    pub fn script(&self, agent_id: &str, s: FakeScript) {
        lock(&self.scripts).insert(agent_id.to_string(), s);
    }

    /// Jobs executed so far.
    pub fn jobs(&self) -> Vec<RunJob> {
        lock(&self.jobs).clone()
    }

    /// Steering messages received, as `(run id, message)`.
    pub fn steering(&self) -> Vec<(String, String)> {
        lock(&self.steering).clone()
    }

    /// Runs asked to stop.
    pub fn cancelled(&self) -> Vec<String> {
        lock(&self.cancelled).clone()
    }

    /// Lets a held run continue.
    pub fn release(&self, run_id: &str) {
        let g = lock(&self.gates)
            .entry(run_id.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone();
        g.notify_one();
    }

    fn script_for(&self, agent: &str) -> FakeScript {
        let s = lock(&self.scripts);
        s.get(agent)
            .or_else(|| s.get("*"))
            .cloned()
            .unwrap_or_default()
    }

    async fn work(
        &self,
        job: &RunJob,
        r: &Arc<dyn RunReporter>,
        s: &FakeScript,
    ) -> Result<RunOutcome> {
        let run = &job.run;
        let agent = run.agent.clone().unwrap_or_default();
        let cx = Cx::new(agent.id.clone(), PrincipalKind::Agent).with_chain(job.chain.clone());
        let task = run.task.clone().unwrap_or_default();
        let kind = AgentTaskKind::try_from(task.kind).unwrap_or(AgentTaskKind::Unspecified);
        let mut out = RunOutcome {
            model: if run.model.is_empty() {
                "fake-model".into()
            } else {
                run.model.clone()
            },
            ..RunOutcome::default()
        };
        let mut log = format!("run {} for {}\n", run.id, agent.id);
        r.report(RunnerEvent::Log(format!(
            "starting {} task",
            kind.as_str_name()
        )))
        .await?;
        r.report(RunnerEvent::ToolCall {
            name: "read_instructions".into(),
            payload: serde_json::json!({"bytes": job.instructions.len()}).to_string(),
        })
        .await?;
        let (Some(changes), Some(repo)) = (&self.changes, &self.repo) else {
            out.summary = "done (no repo access configured)".into();
            out.cost = self.cost(job, s, 0);
            return Ok(out);
        };
        if kind == AgentTaskKind::AdoptEvaluate {
            // The context it was given, as a coding agent would read it back.
            r.report(RunnerEvent::Log(job.prompt.clone())).await?;
            let o = Self::adopt(changes, job, &cx, &agent, s).await?;
            r.report(RunnerEvent::ToolCall {
                name: "adopt_evaluate".into(),
                payload: serde_json::json!({"post": task.post_id, "outcome": o.summary()})
                    .to_string(),
            })
            .await?;
            out.summary = o.summary();
            out.cost = self.cost(job, s, 0);
            r.report(RunnerEvent::Cost(out.cost)).await?;
            log.push_str(&out.summary);
            out.log = log.into_bytes();
            out.transcript =
                format!("user: {}\nassistant: {}\n", job.prompt, out.summary).into_bytes();
            return Ok(out);
        }
        if kind == AgentTaskKind::Review {
            let n: u64 = task.change_request.parse().unwrap_or(0);
            let cr = changes.get_by_number(&cx, &job.repo, n).await?;
            let verdict = s.verdict.unwrap_or(Verdict::Approve);
            changes
                .submit_review(
                    &cx,
                    &job.repo,
                    &cr.id,
                    0,
                    verdict,
                    "Reviewed by the fake runner.",
                    vec![],
                )
                .await?;
            r.report(RunnerEvent::ToolCall {
                name: "submit_review".into(),
                payload: serde_json::json!({"change_request": n, "verdict": verdict.as_str_name()})
                    .to_string(),
            })
            .await?;
            out.summary = format!("Reviewed #{n}: {}", verdict.as_str_name());
            out.cost = self.cost(job, s, 0);
            out.log = log.into_bytes();
            return Ok(out);
        }
        // Lease a path, routing around paths others hold.
        let candidates = if s.paths.is_empty() {
            vec![format!("agent/{}.md", run.id.to_lowercase())]
        } else {
            s.paths.clone()
        };
        let mut chosen = None;
        for p in &candidates {
            match &self.leases {
                Some(l) => match l
                    .acquire(
                        &cx,
                        AcquireRequest {
                            repo_id: job.repo.id.clone(),
                            org_id: job.repo.org_id.clone(),
                            ref_name: run.base_ref.clone(),
                            paths: vec![p.as_bytes().to_vec()],
                            mode: LeaseMode::Exclusive,
                            ttl_seconds: 3600,
                            note: format!("agent run {}", run.id),
                            ..AcquireRequest::default()
                        },
                    )
                    .await
                {
                    Ok((lease, _)) => {
                        r.report(RunnerEvent::LeaseAcquired(lease.id.clone()))
                            .await?;
                        out.lease_ids.push(lease.id);
                        chosen = Some(p.clone());
                        break;
                    }
                    Err(e) if e.reason == ErrorReason::Leased => {
                        let msg = format!("{p} is leased by another agent; routing around it");
                        log.push_str(&msg);
                        log.push('\n');
                        r.report(RunnerEvent::Log(msg)).await?;
                    }
                    Err(e) => return Err(e),
                },
                None => {
                    chosen = Some(p.clone());
                    break;
                }
            }
        }
        let Some(path) = chosen else {
            out.status = RunStatus::Failed("LEASED".into());
            out.summary = "every candidate path is leased by another agent".into();
            out.cost = self.cost(job, s, 0);
            out.log = log.into_bytes();
            return Ok(out);
        };
        let mut parent = repo.resolve(&job.repo, &run.base_ref).await?;
        let mut cr_for_revise = None;
        if kind == AgentTaskKind::Revise {
            let n: u64 = task.change_request.parse().unwrap_or(0);
            let cr = changes.get_by_number(&cx, &job.repo, n).await?;
            if let Some(h) = cr.revisions.last().and_then(|r| r.head.clone()) {
                parent = h;
            }
            cr_for_revise = Some(cr);
        }
        let content = format!(
            "{}\n\nprompt: {}\n",
            match kind {
                AgentTaskKind::Issue => format!("fix for issue #{}", task.issue),
                AgentTaskKind::Revise => format!("revision for #{}", task.change_request),
                _ => "agent change".to_string(),
            },
            crate::ids::hex(blake3::hash(job.prompt.as_bytes()).as_bytes())
        );
        let message = match kind {
            AgentTaskKind::Issue => {
                let title = job
                    .issue
                    .as_ref()
                    .map(|i| i.title.clone())
                    .unwrap_or_default();
                format!("Fix #{}: {title}", task.issue)
            }
            AgentTaskKind::Revise => format!("Address review on #{}", task.change_request),
            _ => format!("Agent run {}", run.id),
        };
        let cs = repo
            .commit(
                &job.repo,
                &parent,
                vec![(path.clone(), Some(content.into_bytes()))],
                &message,
                &job.chain,
            )
            .await?;
        r.report(RunnerEvent::ChangesetCreated(cs.clone())).await?;
        out.changesets.push(cs.clone());
        if let Some(cr) = cr_for_revise {
            changes.revise(&cx, &job.repo, &cr.id, &cs).await?;
            out.summary = format!("Revised #{}", cr.number);
        } else if kind == AgentTaskKind::Issue && !s.no_change_request {
            let n: u64 = task.issue.parse().unwrap_or(0);
            let cr = changes
                .open(
                    &cx,
                    &job.repo,
                    NewChange {
                        target_ref: run.base_ref.clone(),
                        head: Some(cs.clone()),
                        title: message.clone(),
                        body: format!("Fixes #{n}\n\nOpened by agent run {}.", run.id),
                        linked_issues: vec![n],
                        ..NewChange::default()
                    },
                )
                .await?;
            r.report(RunnerEvent::ChangeRequestOpened(cr.number))
                .await?;
            out.change_requests.push(cr.number);
            out.summary = format!("Opened #{} for issue #{n}", cr.number);
        } else {
            out.summary = format!("Committed {path}");
        }
        out.cost = self.cost(job, s, out.changesets.len() as u64);
        r.report(RunnerEvent::Cost(out.cost)).await?;
        log.push_str(&out.summary);
        out.log = log.into_bytes();
        out.transcript = format!("user: {}\nassistant: {}\n", job.prompt, out.summary).into_bytes();
        Ok(out)
    }

    /// An adopt-evaluate task: "not relevant" when scripted, else an issue inspired by the
    /// post, checked as any caller's link is ([`crate::posts::Posts::inspiration`]).
    async fn adopt(
        changes: &Changes,
        job: &RunJob,
        cx: &Cx,
        agent: &bee_proto::v1::Principal,
        s: &FakeScript,
    ) -> Result<Outcome> {
        if let Some(reason) = &s.not_relevant {
            return Ok(Outcome::NotRelevant(reason.clone()));
        }
        let task = job.run.task.clone().unwrap_or_default();
        let posts = changes.posts().ok_or_else(|| {
            crate::service::unserved(
                "ADOPT_EVALUATE runs",
                "the posts module (crate::posts::Posts::attach)",
            )
        })?;
        let post = posts.inspiration(&changes.orgs, cx, &task.post_id).await?;
        let title = format!("Adopt: {}", post.change_title);
        let issue = changes
            .work
            .create_issue(
                cx,
                &RepoInfo::from(&job.repo),
                &Actor {
                    principal: agent.clone(),
                    triage: false,
                    admin: false,
                    association: "NONE".into(),
                },
                NewIssue {
                    title: title.clone(),
                    body: format!(
                        "Change #{} of the post that inspired this, \"{}\", may apply here.\n\nOpened by agent run {}.",
                        post.change_number, post.change_title, job.run.id
                    ),
                    inspired_by: Some(post),
                    ..NewIssue::default()
                },
            )
            .await?;
        Ok(Outcome::Issue(issue.number, issue.title))
    }

    fn cost(&self, job: &RunJob, s: &FakeScript, changesets: u64) -> Cost {
        if let Some(c) = s.cost {
            return c;
        }
        let input = (job.prompt.len() + job.instructions.len()) as u64 + 100;
        let output = 200 + 300 * changesets;
        Cost {
            input_tokens: input,
            output_tokens: output,
            usd_micros: input * 3 + output * 15,
        }
    }
}

#[async_trait]
impl AgentRunner for FakeRunner {
    async fn execute(&self, job: RunJob, reporter: Arc<dyn RunReporter>) -> Result<RunOutcome> {
        lock(&self.jobs).push(job.clone());
        let agent = job
            .run
            .agent
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        let s = self.script_for(&agent);
        if s.hold {
            let g = lock(&self.gates)
                .entry(job.run.id.clone())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone();
            g.notified().await;
        }
        if lock(&self.cancelled).contains(&job.run.id) {
            return Ok(RunOutcome {
                status: RunStatus::Cancelled,
                ..RunOutcome::default()
            });
        }
        if let Some(reason) = &s.fail {
            reporter
                .report(RunnerEvent::Error(format!("scripted failure: {reason}")))
                .await?;
            return Ok(RunOutcome {
                status: RunStatus::Failed(reason.clone()),
                summary: format!("failed: {reason}"),
                cost: self.cost(&job, &s, 0),
                ..RunOutcome::default()
            });
        }
        match self.work(&job, &reporter, &s).await {
            Ok(o) => Ok(o),
            Err(e) if e.reason == ErrorReason::InvalidState && e.subcode == "run_finished" => {
                Ok(RunOutcome {
                    status: RunStatus::Cancelled,
                    ..RunOutcome::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    async fn cancel(&self, run_id: &str) -> Result<()> {
        lock(&self.cancelled).push(run_id.to_string());
        let g = lock(&self.gates).get(run_id).cloned();
        if let Some(g) = g {
            g.notify_one();
        }
        Ok(())
    }

    async fn steer(&self, run_id: &str, message: &str) -> Result<()> {
        lock(&self.steering).push((run_id.to_string(), message.to_string()));
        Ok(())
    }
}
