//! What starts runs, and what the agents module learns from other modules' events.
//!
//! | event | effect |
//! |---|---|
//! | `issue.assigned_to_agent` | an issue run with the run id the issues module assigned |
//! | `issue_comment.created`, `cr_comment.created` with `@agent` | a run for the mentioned agent (issue, review or revise task) |
//! | `cr.opened`, `cr.revised`, `cr.review_requested` | automatic review runs per the repo's review config; review runs for agents requested as reviewers |
//! | `cr.opened/landed/closed/reopened` by an agent | the agent's change request record (dashboards, open CR counts, reverts) |
//! | `cr.landed`, `cr.closed` | waiting runs whose change requests all finished settle |
//! | `review.submitted` | the agent's review record; "request changes" on a waiting run's CR starts a revise run |
//! | `cr.landed`, `ref.updated` on the default ref | debounced watcher triggers for the changed paths; prebuilds |
//! | `post.published` | debounced feed triggers of agents following the post's repo ([`super::feed`]) |
//!
//! Every handler is idempotent (the event log delivers at least once): runs started by events
//! carry an idempotency key derived from the event subject.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    AgentReviewTrigger, AgentRunBudget, AgentTask, AgentTaskKind, ChangeRequest, Comment, Event,
    PrincipalKind, Repo, Review, Verdict, WatcherState, event,
};

use super::model::{AgentCrRec, AgentReviewRec, DefRec, TipRec, TriggerRec, WatcherRec};
use super::registry::{locate, put_locator};
use super::runs::{StartRun, Trigger};
use super::*;
use crate::audit::AuditDraft;
use crate::error::Result;
use crate::events::EventListener;
use crate::store::{Row, Scan};
use crate::work::AgentGate;

/// Feeds forge events to [`Agents`].
pub struct AgentListener {
    agents: Arc<Agents>,
}

impl AgentListener {
    /// A listener over `agents`.
    pub fn new(agents: Arc<Agents>) -> AgentListener {
        AgentListener { agents }
    }
}

#[async_trait]
impl EventListener for AgentListener {
    async fn on_event(&self, e: &Event) {
        let _ = self.agents.on_event(e).await;
    }
}

/// The issues module's [`AgentGate`]: an agent takes work when it has an enabled definition
/// in the repo's org; `BUDGET_EXHAUSTED` when its owner's budget is spent.
pub struct RegistryGate {
    agents: Arc<Agents>,
}

impl RegistryGate {
    /// A gate over `agents`.
    pub fn new(agents: Arc<Agents>) -> RegistryGate {
        RegistryGate { agents }
    }
}

#[async_trait]
impl AgentGate for RegistryGate {
    async fn admit(&self, repo: &Repo, agent: &bee_proto::v1::Principal) -> Result<bool> {
        let cx = Cx::system();
        let Ok(def) = self.agents.definition(&cx, &repo.org_id, &agent.id).await else {
            return Ok(false);
        };
        if !def.enabled {
            return Ok(false);
        }
        self.agents
            .check_owner_budget(&cx, &repo.org_id, def.owner())
            .await?;
        Ok(true)
    }
}

fn chain_of(e: &Event) -> Vec<String> {
    let mut v = vec![e.principal.clone()];
    v.extend(e.on_behalf_of.iter().cloned());
    v.retain(|p| !p.is_empty());
    v
}

fn json(e: &Event) -> serde_json::Value {
    serde_json::from_str(&e.payload_json).unwrap_or_default()
}

fn change_of(e: &Event) -> Option<ChangeRequest> {
    match &e.body {
        Some(event::Body::Change(c)) => Some(c.clone()),
        _ => None,
    }
}

impl Agents {
    /// Handles one forge event (see the module docs).
    pub async fn on_event(self: &Arc<Self>, e: &Event) -> Result<()> {
        match e.kind.as_str() {
            "issue.assigned_to_agent" => self.on_assigned(e).await,
            "issue_comment.created" => self.on_issue_comment(e).await,
            "cr_comment.created" => self.on_cr_comment(e).await,
            "cr.opened" | "cr.revised" | "cr.review_requested" | "cr.reopened" | "cr.edited" => {
                self.on_cr_open(e).await
            }
            "cr.landed" | "cr.closed" => self.on_cr_done(e).await,
            "review.submitted" => self.on_review(e).await,
            "ref.updated" => self.on_ref_updated(e).await,
            "post.published" => self.on_post_published(e).await,
            _ => Ok(()),
        }
    }

    async fn repo(&self, id: &str) -> Result<Repo> {
        self.forge.orgs.repo_any(&Cx::system(), id).await
    }

    async fn on_assigned(self: &Arc<Self>, e: &Event) -> Result<()> {
        let v = json(e);
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let repo = self.repo(&s("repo_id")).await?;
        let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        let agent = v
            .get("agent")
            .and_then(|a| a.get("id"))
            .and_then(|a| a.as_str())
            .unwrap_or("")
            .to_string();
        let run_id = s("agent_run_id");
        let res = self
            .start_run(
                &Cx::system(),
                StartRun {
                    repo: repo.clone(),
                    task: AgentTask {
                        kind: AgentTaskKind::Issue as i32,
                        issue: number.to_string(),
                        prompt: s("instructions"),
                        ..Default::default()
                    },
                    agent_id: agent,
                    base_ref: s("base_ref"),
                    trigger: Trigger::IssueAssignment,
                    trigger_ref: s("issue_id"),
                    run_id: run_id.clone(),
                    chain: chain_of(e),
                    ..StartRun::default()
                },
            )
            .await;
        if let Err(err) = res {
            // The issue recorded a run that cannot start: end it with the reason.
            let _ = self
                .forge
                .work
                .finish_agent_run(
                    &Cx::system(),
                    &crate::work::RepoInfo::from(&repo),
                    number,
                    &run_id,
                    &err.reason_name().to_lowercase(),
                )
                .await;
        }
        Ok(())
    }

    /// Enabled agents mentioned in `body` (by login or id), with their definitions.
    async fn mentioned(&self, repo: &Repo, body: &str) -> Vec<AgentDefinition> {
        let mut out: Vec<AgentDefinition> = Vec::new();
        for h in crate::work::text::mentions(body).into_iter().take(20) {
            let dir = &self.forge.hooks.directory;
            let p = match dir.get(&h).await {
                Some(p) => Some(p),
                None => dir.by_login(&h).await,
            };
            let Some(p) = p.filter(|p| p.kind == PrincipalKind::Agent as i32) else {
                continue;
            };
            if let Ok(d) = self.definition(&Cx::system(), &repo.org_id, &p.id).await
                && d.enabled
                && !out.iter().any(|x| x.agent.id == d.agent.id)
            {
                out.push(d);
            }
        }
        out
    }

    async fn on_issue_comment(self: &Arc<Self>, e: &Event) -> Result<()> {
        let v = json(e);
        let comment: Comment =
            serde_json::from_value(v.get("comment").cloned().unwrap_or_default())
                .unwrap_or_default();
        let number = v
            .get("issue")
            .and_then(|i| i.get("number"))
            .and_then(|n| {
                n.as_u64()
                    .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0);
        if number == 0 || comment.body.is_empty() {
            return Ok(());
        }
        let repo = self.repo(&e.repo_id).await?;
        let author = comment
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        for d in self.mentioned(&repo, &comment.body).await {
            if d.agent.id == author {
                continue;
            }
            let _ = self
                .start_run(
                    &Cx::system(),
                    StartRun {
                        repo: repo.clone(),
                        task: AgentTask {
                            kind: AgentTaskKind::Issue as i32,
                            issue: number.to_string(),
                            prompt: comment.body.clone(),
                            ..Default::default()
                        },
                        agent_id: d.agent.id.clone(),
                        trigger: Trigger::Mention,
                        trigger_ref: comment.id.clone(),
                        idempotency_key: format!("mention:{}:{}", comment.id, d.agent.id),
                        chain: chain_of(e),
                        ..StartRun::default()
                    },
                )
                .await;
        }
        Ok(())
    }

    async fn on_cr_comment(self: &Arc<Self>, e: &Event) -> Result<()> {
        let Some(cr) = change_of(e) else {
            return Ok(());
        };
        let comment: Comment = serde_json::from_str(&e.payload_json).unwrap_or_default();
        if comment.body.is_empty() {
            return Ok(());
        }
        let repo = self.repo(&e.repo_id).await?;
        let author = comment
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        let cr_author = cr.author.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        for d in self.mentioned(&repo, &comment.body).await {
            if d.agent.id == author {
                continue;
            }
            let kind = if d.agent.id == cr_author {
                AgentTaskKind::Revise
            } else {
                AgentTaskKind::Review
            };
            let _ = self
                .start_run(
                    &Cx::system(),
                    StartRun {
                        repo: repo.clone(),
                        task: AgentTask {
                            kind: kind as i32,
                            change_request: cr.number.to_string(),
                            prompt: comment.body.clone(),
                            ..Default::default()
                        },
                        agent_id: d.agent.id.clone(),
                        base_ref: cr.target_ref.clone(),
                        trigger: Trigger::Mention,
                        trigger_ref: comment.id.clone(),
                        idempotency_key: format!("mention:{}:{}", comment.id, d.agent.id),
                        chain: chain_of(e),
                        ..StartRun::default()
                    },
                )
                .await;
        }
        Ok(())
    }

    /// Records an agent-authored CR's state.
    async fn note_agent_cr(&self, repo_id: &str, cr: &ChangeRequest, state: &str) -> Result<()> {
        let Some(a) = cr
            .author
            .as_ref()
            .filter(|a| a.kind == PrincipalKind::Agent as i32)
        else {
            return Ok(());
        };
        let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
        let now = tx.now_ms();
        let cur = tx.get_msg::<AgentCrRec>(&AGENT_CRS, &a.id, &cr.id).await?;
        let mut rec = cur.as_ref().map(|(r, _)| r.clone()).unwrap_or(AgentCrRec {
            repo_id: repo_id.to_string(),
            cr_id: cr.id.clone(),
            number: cr.number,
            created_ms: cr.created_at.as_ref().map(crate::ids::ts_ms).unwrap_or(now),
            ..Default::default()
        });
        if !cr.title.is_empty() {
            rec.title = cr.title.clone();
        }
        rec.number = cr.number;
        rec.state = state.to_string();
        let row = Row::new(a.id.clone(), cr.id.clone())
            .k1(repo_id)
            .n1(rec.created_ms as i64)
            .msg(&rec);
        match cur {
            Some((_, r)) => {
                tx.update(&AGENT_CRS, &row.version(r.version)).await?;
            }
            None => {
                tx.insert(&AGENT_CRS, &row).await?;
                put_locator(&mut tx, "agent_cr", &cr.id, &a.id).await?;
            }
        }
        // A revert marks the reverted change of its agent.
        if !cr.revert_of.is_empty()
            && let Some(agent) = locate(&mut tx, "agent_cr", &cr.revert_of).await?
            && let Some((mut r, row)) = tx
                .get_msg::<AgentCrRec>(&AGENT_CRS, &agent, &cr.revert_of)
                .await?
            && !r.reverted
            && state == "landed"
        {
            r.reverted = true;
            tx.update(
                &AGENT_CRS,
                &Row::new(agent.clone(), cr.revert_of.clone())
                    .k1(r.repo_id.clone())
                    .n1(r.created_ms as i64)
                    .version(row.version)
                    .msg(&r),
            )
            .await?;
        }
        tx.commit().await
    }

    async fn on_cr_open(self: &Arc<Self>, e: &Event) -> Result<()> {
        let Some(cr) = change_of(e) else {
            return Ok(());
        };
        if cr
            .author
            .as_ref()
            .is_some_and(|a| a.kind == PrincipalKind::Agent as i32)
        {
            self.note_agent_cr(
                &e.repo_id,
                &cr,
                if cr.state == bee_proto::v1::ChangeState::Landed as i32 {
                    "landed"
                } else {
                    "open"
                },
            )
            .await?;
        }
        if e.kind == "cr.edited" || e.kind == "cr.reopened" {
            return Ok(());
        }
        let repo = self.repo(&e.repo_id).await?;
        let rev = cr.revisions.last().map(|r| r.n).unwrap_or(1);
        let author = cr.author.clone().unwrap_or_default();
        // Agents requested as reviewers.
        let mut reviewers: Vec<String> = Vec::new();
        if e.kind == "cr.review_requested" || e.kind == "cr.opened" {
            for r in &cr.review_requests {
                if let Some(p) = r
                    .reviewer
                    .as_ref()
                    .filter(|p| p.kind == PrincipalKind::Agent as i32)
                {
                    reviewers.push(p.id.clone());
                }
            }
        }
        // The automatic review configuration.
        let cfg = self.review_config(&Cx::system(), &repo).await?;
        let trig =
            AgentReviewTrigger::try_from(cfg.trigger).unwrap_or(AgentReviewTrigger::Unspecified);
        let fires = match trig {
            AgentReviewTrigger::OnOpen => e.kind == "cr.opened",
            AgentReviewTrigger::OnEveryRevision => e.kind == "cr.opened" || e.kind == "cr.revised",
            AgentReviewTrigger::OnRequest => reviewers.contains(&cfg.agent_principal_id),
            _ => false,
        };
        // Policy hook (F49-009): a land rule with `require_agent_review` makes the configured
        // review agent review every opened change request and every new revision, whatever
        // the configured trigger, drafts, authors and paths.
        let required = matches!(e.kind.as_str(), "cr.opened" | "cr.revised")
            && match self.changes.effective_policy(&repo).await {
                Ok(p) => p.requires_agent_review(&cr.target_ref),
                Err(_) => false,
            };
        let mut config_run = false;
        let by_config = fires
            && (!cr.draft || cfg.include_drafts)
            && (author.kind != PrincipalKind::Agent as i32 || cfg.review_agent_authored)
            && self.touches(&repo, &cr, &cfg.paths).await;
        if (required || by_config)
            && !cfg.agent_principal_id.is_empty()
            && author.id != cfg.agent_principal_id
        {
            config_run = true;
            let _ = self
                .start_run(
                    &Cx::system(),
                    StartRun {
                        repo: repo.clone(),
                        task: AgentTask {
                            kind: AgentTaskKind::Review as i32,
                            change_request: cr.number.to_string(),
                            prompt: cfg.instructions.clone(),
                            ..Default::default()
                        },
                        agent_id: cfg.agent_principal_id.clone(),
                        runtime_id: cfg.runtime_id.clone(),
                        base_ref: cr.target_ref.clone(),
                        budget: cfg.budget,
                        trigger: Trigger::ReviewConfig,
                        trigger_ref: cr.id.clone(),
                        idempotency_key: format!(
                            "review:{}:{}:{}",
                            cr.id, cfg.agent_principal_id, rev
                        ),
                        chain: chain_of(e),
                        ..StartRun::default()
                    },
                )
                .await;
        }
        for agent in reviewers {
            if config_run && agent == cfg.agent_principal_id {
                continue;
            }
            let Ok(d) = self.definition(&Cx::system(), &repo.org_id, &agent).await else {
                continue;
            };
            if !d.enabled || !d.toolsets.iter().any(|t| t == "review") {
                continue;
            }
            let _ = self
                .start_run(
                    &Cx::system(),
                    StartRun {
                        repo: repo.clone(),
                        task: AgentTask {
                            kind: AgentTaskKind::Review as i32,
                            change_request: cr.number.to_string(),
                            ..Default::default()
                        },
                        agent_id: agent.clone(),
                        base_ref: cr.target_ref.clone(),
                        trigger: Trigger::ReviewConfig,
                        trigger_ref: cr.id.clone(),
                        idempotency_key: format!("review:{}:{}:{}", cr.id, agent, rev),
                        chain: chain_of(e),
                        ..StartRun::default()
                    },
                )
                .await;
        }
        Ok(())
    }

    /// Whether the CR's latest revision touches `paths` (empty = everything).
    async fn touches(&self, repo: &Repo, cr: &ChangeRequest, paths: &[Vec<u8>]) -> bool {
        if paths.is_empty() {
            return true;
        }
        let Some(r) = cr.revisions.last() else {
            return false;
        };
        let (Some(base), Some(head)) = (&r.base, &r.head) else {
            return false;
        };
        let Ok(changed) = self.ports.repo.changed_paths(repo, base, head).await else {
            return false;
        };
        changed.iter().any(|c| {
            paths
                .iter()
                .any(|p| crate::leases::covers(&String::from_utf8_lossy(p), c))
        })
    }

    async fn on_cr_done(self: &Arc<Self>, e: &Event) -> Result<()> {
        let Some(cr) = change_of(e) else {
            return Ok(());
        };
        let landed = e.kind == "cr.landed";
        self.note_agent_cr(&e.repo_id, &cr, if landed { "landed" } else { "closed" })
            .await?;
        for run in self.runs_of_cr(&e.repo_id, &cr.id).await? {
            let _ = self.settle_waiting(&run).await;
        }
        if landed {
            let repo = self.repo(&e.repo_id).await?;
            if let Some(r) = cr.revisions.last()
                && let (Some(base), Some(head)) = (&r.base, &r.head)
                && let Ok(paths) = self.ports.repo.changed_paths(&repo, base, head).await
            {
                self.note_changes(&repo, &paths, head).await?;
            }
            let target = if cr.target_ref.is_empty() {
                repo.default_ref.clone()
            } else {
                cr.target_ref.clone()
            };
            let _ = self
                .workspaces
                .on_ref_advanced(&Cx::system(), &repo, &target)
                .await;
        }
        Ok(())
    }

    async fn on_review(self: &Arc<Self>, e: &Event) -> Result<()> {
        let Some(event::Body::Review(r)) = &e.body else {
            return Ok(());
        };
        let r: &Review = r;
        let reviewer = r.reviewer.clone().unwrap_or_default();
        let verdict = Verdict::try_from(r.verdict).unwrap_or(Verdict::Unspecified);
        if reviewer.kind == PrincipalKind::Agent as i32 {
            let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
            let now = tx.now_ms();
            let rec = AgentReviewRec {
                repo_id: e.repo_id.clone(),
                cr_id: r.change_id.clone(),
                verdict: verdict
                    .as_str_name()
                    .trim_start_matches("VERDICT_")
                    .to_string(),
                created_ms: r.created_at.as_ref().map(crate::ids::ts_ms).unwrap_or(now),
            };
            tx.put(
                &AGENT_REVIEWS,
                &Row::new(reviewer.id.clone(), r.id.clone())
                    .k1(e.repo_id.clone())
                    .n1(rec.created_ms as i64)
                    .msg(&rec),
            )
            .await?;
            tx.commit().await?;
        }
        if verdict != Verdict::RequestChanges {
            return Ok(());
        }
        // Changes requested on a waiting run's CR: the agent revises.
        let repo = self.repo(&e.repo_id).await?;
        for run_id in self.runs_of_cr(&e.repo_id, &r.change_id).await? {
            let Ok(rec) = self.run_rec(&Cx::system(), &run_id).await else {
                continue;
            };
            let run = rec.run();
            if run.state != bee_proto::v1::AgentRunState::WaitingForReview as i32 {
                continue;
            }
            let agent = run.agent.clone().unwrap_or_default();
            if agent.id == reviewer.id {
                continue;
            }
            let Ok(cr) = self.changes.get(&Cx::system(), &repo, &r.change_id).await else {
                continue;
            };
            let _ = self
                .start_run(
                    &Cx::system(),
                    StartRun {
                        repo: repo.clone(),
                        task: AgentTask {
                            kind: AgentTaskKind::Revise as i32,
                            change_request: cr.number.to_string(),
                            prompt: r.body.clone(),
                            ..Default::default()
                        },
                        agent_id: agent.id.clone(),
                        runtime_id: run.runtime_id.clone(),
                        model: run.model.clone(),
                        base_ref: run.base_ref.clone(),
                        budget: run.budget,
                        trigger: Trigger::Revise,
                        trigger_ref: r.id.clone(),
                        idempotency_key: format!("revise:{}:{}", r.id, agent.id),
                        // On behalf of the same root as the original run.
                        chain: rec.chain.iter().skip(1).cloned().collect(),
                        ..StartRun::default()
                    },
                )
                .await;
        }
        Ok(())
    }

    async fn on_ref_updated(self: &Arc<Self>, e: &Event) -> Result<()> {
        let Some(event::Body::RefUpdated(r)) = &e.body else {
            return Ok(());
        };
        let Ok(repo) = self.repo(&e.repo_id).await else {
            return Ok(());
        };
        let Some(target) = r.target.clone() else {
            return Ok(());
        };
        if r.name == repo.default_ref {
            let prev = {
                let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
                let prev = tx
                    .get_msg::<TipRec>(&REF_TIPS, &repo.id, &r.name)
                    .await?
                    .and_then(|(t, _)| t.target);
                tx.put(
                    &REF_TIPS,
                    &Row::new(repo.id.clone(), r.name.clone()).msg(&TipRec {
                        target: Some(target.clone()),
                    }),
                )
                .await?;
                tx.commit().await?;
                prev
            };
            if let Some(prev) = prev
                && prev != target
                && let Ok(paths) = self.ports.repo.changed_paths(&repo, &prev, &target).await
            {
                self.note_changes(&repo, &paths, &target).await?;
            }
        }
        let _ = self
            .workspaces
            .on_ref_advanced(&Cx::system(), &repo, &r.name)
            .await;
        Ok(())
    }

    /// Folds changed paths into the debounced triggers of the watchers that cover them.
    pub async fn note_changes(
        &self,
        repo: &Repo,
        paths: &[String],
        changeset: &bee_proto::v1::ObjectId,
    ) -> Result<usize> {
        let cx = Cx::system();
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        let watchers = tx
            .scan_msgs::<WatcherRec>(
                &WATCHERS,
                &Scan::scope(repo.id.clone(), crate::store::MAX_SCAN),
            )
            .await?;
        let now = tx.now_ms();
        let mut n = 0;
        for (wr, _) in watchers {
            let w = wr.w();
            if w.state != WatcherState::Active as i32 {
                continue;
            }
            let hit: Vec<String> = paths
                .iter()
                .filter(|p| super::watchers::watches(&w, p))
                .cloned()
                .collect();
            if hit.is_empty() {
                continue;
            }
            let window = if w.check_interval_seconds > 0 {
                w.check_interval_seconds * 1000
            } else {
                self.config.watch_debounce.as_millis() as u64
            };
            let cur = tx.get_msg::<TriggerRec>(&TRIGGERS, "", &w.id).await?;
            let mut t = cur.as_ref().map(|(t, _)| t.clone()).unwrap_or(TriggerRec {
                watcher_id: w.id.clone(),
                repo_id: repo.id.clone(),
                first_ms: now,
                ..Default::default()
            });
            for p in hit {
                if !t.paths.contains(&p) && t.paths.len() < 1000 {
                    t.paths.push(p);
                }
            }
            t.paths.sort();
            if !t.changesets.contains(changeset) && t.changesets.len() < 100 {
                t.changesets.push(changeset.clone());
            }
            t.changes += 1;
            // Trailing debounce, capped at ten windows after the first change.
            t.due_ms = (now + window).min(t.first_ms + 10 * window);
            let row = Row::new("", w.id.clone())
                .k1(repo.id.clone())
                .n1(t.due_ms as i64)
                .msg(&t);
            match cur {
                Some((_, r)) => {
                    tx.update(&TRIGGERS, &row.version(r.version)).await?;
                }
                None => tx.insert(&TRIGGERS, &row).await?,
            }
            n += 1;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// Fires watcher triggers whose debounce window has passed.
    pub(crate) async fn fire_due_triggers(self: &Arc<Self>, cx: &Cx) -> Result<usize> {
        let due = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let now = tx.now_ms() as i64;
            tx.scan_msgs::<TriggerRec>(&TRIGGERS, &Scan::scope("", 100).by_n1().n1_max(now))
                .await?
        };
        let mut fired = 0;
        for (t, row) in due {
            // Claim it.
            let (wrec, budget) = {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                if tx.get(&TRIGGERS, "", &row.id).await?.map(|r| r.version) != Some(row.version) {
                    continue;
                }
                tx.delete(&TRIGGERS, "", &row.id).await?;
                let Some((mut wr, wrow)) = tx
                    .get_msg::<WatcherRec>(&WATCHERS, &t.repo_id, &t.watcher_id)
                    .await?
                else {
                    tx.commit().await?;
                    continue;
                };
                let w = wr.w();
                let day = utc_date(tx.now_ms());
                if wr.day != day {
                    wr.day = day;
                    wr.day_runs = 0;
                    wr.day_usd_micros = 0;
                }
                let b = w.budget.unwrap_or_default();
                let spent = b.max_usd_micros > 0 && wr.day_usd_micros >= b.max_usd_micros;
                let repo_id = t.repo_id.clone();
                let json = serde_json::json!({
                    "watcher_id": w.id, "paths": t.paths, "changes": t.changes,
                    "skipped": if spent { "daily budget spent" } else { "" },
                })
                .to_string();
                tx.emit_event(Event {
                    kind: "watcher.triggered".into(),
                    repo_id: repo_id.clone(),
                    org_id: wr.org_id.clone(),
                    subject: Some(bee_proto::v1::SubjectRef {
                        kind: bee_proto::v1::SubjectKind::Repo as i32,
                        repo: super::registry::repo_ref_id(&repo_id),
                        id: w.id.clone(),
                    }),
                    payload_json: json.clone(),
                    ..Default::default()
                })
                .await?;
                tx.audit(
                    AuditDraft::new("agent.watcher_trigger", &wr.org_id)
                        .repo_id(&repo_id)
                        .target_id(&w.id)
                        .after(&json),
                )
                .await?;
                if !spent {
                    wr.day_runs += 1;
                }
                tx.update(
                    &WATCHERS,
                    &Row::new(repo_id.clone(), w.id.clone())
                        .k1(lc(&w.name))
                        .n1(wrow.n1.unwrap_or(0))
                        .version(wrow.version)
                        .msg(&wr),
                )
                .await?;
                tx.commit().await?;
                if spent || w.state != WatcherState::Active as i32 {
                    continue;
                }
                let remaining = AgentRunBudget {
                    max_usd_micros: b.max_usd_micros.saturating_sub(wr.day_usd_micros),
                    ..b
                };
                (wr, remaining)
            };
            let w = wrec.w();
            let Ok(repo) = self.repo(&t.repo_id).await else {
                continue;
            };
            let agent = w.agent.clone().unwrap_or_default();
            let prompt = format!(
                "You watch the component {} ({}) in {}. These paths changed:\n{}\n\nCheck the component's telemetry{} for regressions and report anomalies with evidence.{}",
                if w.component.is_empty() {
                    &w.name
                } else {
                    &w.component
                },
                w.service_name,
                repo.full_name,
                t.paths
                    .iter()
                    .map(|p| format!("- {p}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                w.telemetry
                    .as_ref()
                    .map(|s| format!(" (metrics: {})", s.metric_names.join(", ")))
                    .unwrap_or_default(),
                if w.instructions.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{}", w.instructions)
                },
            );
            let key = format!("watch:{}:{}", w.id, t.due_ms);
            if self
                .start_run(
                    cx,
                    StartRun {
                        repo,
                        task: AgentTask {
                            kind: AgentTaskKind::Freeform as i32,
                            prompt,
                            ..Default::default()
                        },
                        agent_id: agent.id.clone(),
                        runtime_id: w.runtime_id.clone(),
                        budget: Some(budget),
                        trigger: Trigger::Watcher,
                        trigger_ref: w.id.clone(),
                        idempotency_key: key,
                        chain: vec![wrec.created_by.clone()],
                        watcher_id: w.id.clone(),
                        ..StartRun::default()
                    },
                )
                .await
                .is_ok()
            {
                fired += 1;
            }
        }
        Ok(fired)
    }

    /// Adds a finished watcher run's cost to the watcher's daily counters.
    pub(crate) async fn charge_watcher(
        &self,
        repo_id: &str,
        watcher_id: &str,
        usd: u64,
    ) -> Result<()> {
        if usd == 0 {
            return Ok(());
        }
        let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
        let Some((mut wr, row)) = tx
            .get_msg::<WatcherRec>(&WATCHERS, repo_id, watcher_id)
            .await?
        else {
            return Ok(());
        };
        let day = utc_date(tx.now_ms());
        if wr.day != day {
            wr.day = day;
            wr.day_runs = 0;
            wr.day_usd_micros = 0;
        }
        wr.day_usd_micros = wr.day_usd_micros.saturating_add(usd);
        let w = wr.w();
        tx.update(
            &WATCHERS,
            &Row::new(repo_id, watcher_id)
                .k1(lc(&w.name))
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&wr),
        )
        .await?;
        tx.commit().await
    }

    /// Fires due schedules of every agent definition.
    pub(crate) async fn fire_schedules(self: &Arc<Self>, cx: &Cx) -> Result<usize> {
        let defs = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<DefRec>(
                &DEFS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        };
        let now = self.db.now_ms();
        let mut fired = 0;
        for (d, _) in defs {
            if !d.enabled || d.schedules.is_empty() {
                continue;
            }
            let agent_id = d.agent.as_ref().map(|a| a.id.clone()).unwrap_or_default();
            let mut due = Vec::new();
            for s in &d.schedules {
                if let Ok(c) = super::cron::Cron::parse(&s.cron)
                    && let Some(t) = c.next_between(s.last_fired_ms, now)
                {
                    due.push((s.clone(), t));
                }
            }
            if due.is_empty() {
                continue;
            }
            // Advance the schedules first (CAS), so a concurrent dispatcher does not fire twice.
            {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                let Some((mut rec, row)) =
                    tx.get_msg::<DefRec>(&DEFS, &d.org_id, &agent_id).await?
                else {
                    continue;
                };
                for s in rec.schedules.iter_mut() {
                    if due.iter().any(|(x, _)| x.id == s.id) {
                        s.last_fired_ms = now;
                    }
                }
                if tx
                    .update(
                        &DEFS,
                        &Row::new(d.org_id.clone(), agent_id.clone())
                            .k1(lc(&rec.name))
                            .k2(agent_id.clone())
                            .n1(row.n1.unwrap_or(0))
                            .version(row.version)
                            .msg(&rec),
                    )
                    .await
                    .is_err()
                {
                    continue;
                }
                tx.audit(
                    AuditDraft::new("agent.schedule_fire", &d.org_id)
                        .target_id(&agent_id)
                        .after(&serde_json::json!({"schedules": due.iter().map(|(s, t)| serde_json::json!({"id": s.id, "at": t})).collect::<Vec<_>>()}).to_string()),
                )
                .await?;
                tx.commit().await?;
            }
            for (s, t) in due {
                let Ok(repo) = self.repo(&s.repo_id).await else {
                    continue;
                };
                if self
                    .start_run(
                        cx,
                        StartRun {
                            repo,
                            task: AgentTask {
                                kind: AgentTaskKind::Freeform as i32,
                                prompt: s.prompt.clone(),
                                ..Default::default()
                            },
                            agent_id: agent_id.clone(),
                            trigger: Trigger::Schedule,
                            trigger_ref: s.id.clone(),
                            idempotency_key: format!("schedule:{}:{t}", s.id),
                            chain: vec![d.owners.first().cloned().unwrap_or_default()],
                            ..StartRun::default()
                        },
                    )
                    .await
                    .is_ok()
                {
                    fired += 1;
                }
            }
        }
        Ok(fired)
    }
}
