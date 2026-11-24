//! Feed triggers: agents follow repos and evaluate the upgrades those repos post
//! (`docs/spec/agents.md` §5 "Feed triggers").
//!
//! - **Config.** A repo maintainer gives one of their agents a feed trigger in their repo
//!   (`SetAgentFeedTrigger`): enabled, a daily USD budget, a daily run cap, and source repos.
//!   The agent principal follows each source (`FollowRepo` as the agent); a source dropped from
//!   the set is unfollowed.
//! - **What fires.** `post.published` in any repo the agent follows (not only the sources) folds
//!   the post into the trigger's pending run: trailing debounce of
//!   [`AgentsConfig::watch_debounce`](super::AgentsConfig) (the trigger's
//!   `check_interval_seconds`), capped at ten windows after the first post, as watchers do. A
//!   post is taken once per agent and repo (the event log delivers at least once), and only
//!   when the agent may read the post's repo; the trigger's own repo is never a source.
//! - **The run.** When the window passes, one `ADOPT_EVALUATE` run starts in the trigger's repo
//!   for every post folded in (`task.post_id` is the first), on behalf of whoever configured the
//!   trigger, with idempotency key `adopt:<repo>:<agent>:<post>`. Once the day's (UTC) budget
//!   or run cap is spent, triggers fire as skipped (`agent_feed_trigger.fired` says why) and
//!   start nothing. The run's cost is charged to the trigger's day.
//! - **The prompt** ([`adopt_prompt`], pure): the posts (text, media URLs signed for the run's
//!   chain, change title), the source change's diff summary and changed paths, the target
//!   repo, and the instructions: decide, then finish with one of the three [`Outcome`]
//!   summaries.

use std::collections::BTreeSet;
use std::sync::Arc;

use bee_proto::v1::{
    AgentFeedTrigger, AgentRunBudget, AgentTask, AgentTaskKind, Event, Post, PostState, Principal,
    PrincipalKind, Repo, RepoRef, SubjectKind, SubjectRef, event, repo_ref,
};

use super::model::{FeedPendingRec, FeedTriggerRec, RunRec};
use super::registry::repo_ref_id;
use super::runs::{StartRun, Trigger};
use super::*;
use crate::audit::AuditDraft;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::posts::adopt::{NOT_READABLE, chain_reads};
use crate::store::{Key, Row, Scan};

/// Most source repos of one trigger.
pub const MAX_SOURCES: usize = 100;
/// Most posts folded into one run.
pub const MAX_FOLDED: usize = 20;
/// Most followers of a repo a published post is matched against.
pub const MAX_FOLLOWERS: usize = 10_000;
/// Most changed paths listed in a prompt.
pub const MAX_PROMPT_PATHS: usize = 200;
/// Largest daily budget, in USD.
pub const MAX_DAILY_USD: f64 = 1_000_000.0;

/// How an ADOPT_EVALUATE run ends: its `AgentRun.summary` takes exactly one of these forms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `not relevant: <reason>`.
    NotRelevant(String),
    /// `opened issue #<n>: <title>`.
    Issue(u64, String),
    /// `opened change request #<n>: <title>`.
    ChangeRequest(u64, String),
}

impl Outcome {
    /// The run summary.
    pub fn summary(&self) -> String {
        match self {
            Outcome::NotRelevant(r) => format!("not relevant: {}", one_line(r)),
            Outcome::Issue(n, t) => format!("opened issue #{n}: {}", one_line(t)),
            Outcome::ChangeRequest(n, t) => format!("opened change request #{n}: {}", one_line(t)),
        }
    }

    /// Parses a run summary written in one of the three forms.
    pub fn parse(s: &str) -> Option<Outcome> {
        if let Some(r) = s.strip_prefix("not relevant: ") {
            return Some(Outcome::NotRelevant(r.to_string()));
        }
        let opened = |rest: &str| -> Option<(u64, String)> {
            let rest = rest.strip_prefix('#')?;
            let (n, t) = rest.split_once(": ")?;
            Some((n.parse().ok()?, t.to_string()))
        };
        if let Some(rest) = s.strip_prefix("opened issue ") {
            let (n, t) = opened(rest)?;
            return Some(Outcome::Issue(n, t));
        }
        if let Some(rest) = s.strip_prefix("opened change request ") {
            let (n, t) = opened(rest)?;
            return Some(Outcome::ChangeRequest(n, t));
        }
        None
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Settings of a feed trigger (`SetAgentFeedTrigger`), sources resolved.
#[derive(Clone, Debug, Default)]
pub struct FeedSettings {
    /// Fires.
    pub enabled: bool,
    /// Daily budget in USD (0 = unlimited).
    pub daily_usd_budget: f64,
    /// Runs per day (0 = unlimited).
    pub max_runs_per_day: u32,
    /// Source repos.
    pub sources: Vec<Repo>,
}

/// The key of a trigger: `<repo id>/<agent id>`.
pub fn trigger_key(repo_id: &str, agent_id: &str) -> String {
    format!("{repo_id}/{agent_id}")
}

/// USD as micros; `INVALID_ARGUMENT` for a negative, non-finite or absurd amount.
pub fn usd_micros(usd: f64) -> Result<u64> {
    if !usd.is_finite() || !(0.0..=MAX_DAILY_USD).contains(&usd) {
        return Err(ForgeError::invalid(
            "invalid_budget",
            format!("daily_usd_budget must be between 0 and {MAX_DAILY_USD}"),
        ));
    }
    Ok((usd * 1_000_000.0).round() as u64)
}

/// Resets the day's counters when `today` is a new UTC day.
pub fn roll_day(rec: &mut FeedTriggerRec, today: &str) {
    if rec.day != today {
        rec.day = today.to_string();
        rec.day_runs = 0;
        rec.day_usd_micros = 0;
    }
}

/// Why a firing trigger starts no run today, if it does not: the run cap or the budget is
/// spent (counters of the current day).
pub fn exhausted(rec: &FeedTriggerRec) -> Option<&'static str> {
    let t = rec.t();
    if t.max_runs_per_day > 0 && rec.day_runs >= u64::from(t.max_runs_per_day) {
        return Some("daily run cap reached");
    }
    let budget = usd_micros(t.daily_usd_budget).unwrap_or(0);
    if budget > 0 && rec.day_usd_micros >= budget {
        return Some("daily budget spent");
    }
    None
}

/// Folds a post into a pending run: trailing debounce of `window_ms`, capped at ten windows
/// after the first post; at most [`MAX_FOLDED`] posts (later ones wait for the next run).
/// `None` when the run is full.
pub fn fold(
    cur: Option<FeedPendingRec>,
    repo_id: &str,
    agent_id: &str,
    post_id: &str,
    now: u64,
    window_ms: u64,
) -> Option<FeedPendingRec> {
    let mut p = cur.unwrap_or(FeedPendingRec {
        repo_id: repo_id.to_string(),
        agent_id: agent_id.to_string(),
        first_ms: now,
        ..Default::default()
    });
    if p.post_ids.iter().any(|x| x == post_id) {
        return Some(p);
    }
    if p.post_ids.len() >= MAX_FOLDED {
        return None;
    }
    p.post_ids.push(post_id.to_string());
    p.due_ms = (now + window_ms).min(p.first_ms + 10 * window_ms);
    Some(p)
}

/// What the prompt says about one post.
#[derive(Clone, Debug, Default)]
pub struct PostContext {
    /// The post.
    pub post: Post,
    /// Full name of its repo.
    pub repo: String,
    /// Media, as `(kind, alt text, URL)`.
    pub media: Vec<(String, String, String)>,
    /// The source change's diff summary.
    pub diff_summary: String,
    /// Paths the source change touched.
    pub paths: Vec<String>,
}

/// One line saying how many paths a change touched and how, from `(path, diff class,
/// deleted)`.
pub fn diff_summary(paths: &[(String, String, bool)]) -> String {
    if paths.is_empty() {
        return "no paths changed".into();
    }
    let deleted = paths.iter().filter(|p| p.2).count();
    let mut classes: std::collections::BTreeMap<&str, usize> = Default::default();
    for (_, c, _) in paths {
        *classes
            .entry(if c.is_empty() { "unclassified" } else { c })
            .or_default() += 1;
    }
    let classes: Vec<String> = classes.iter().map(|(c, n)| format!("{c} {n}")).collect();
    format!(
        "{} path{} changed ({} deleted); diff classes: {}",
        paths.len(),
        if paths.len() == 1 { "" } else { "s" },
        deleted,
        classes.join(", ")
    )
}

/// The ADOPT_EVALUATE prompt: the posts and their changes, the target repo, the instructions.
pub fn adopt_prompt(target: &Repo, posts: &[PostContext]) -> String {
    let mut s = format!(
        "Repos you follow published {}. Decide whether {} applies to {}.\n",
        if posts.len() == 1 {
            "an upgrade".to_string()
        } else {
            format!("{} upgrades", posts.len())
        },
        if posts.len() == 1 { "it" } else { "each" },
        target.full_name
    );
    for c in posts {
        let p = &c.post;
        s.push_str(&format!(
            "\n## Post {} in {}\n\nChange #{}: {}\n\n{}\n",
            p.id,
            c.repo,
            p.change_number,
            p.change_title,
            if p.text.is_empty() {
                "(no text)".to_string()
            } else {
                p.text.clone()
            }
        ));
        if !c.media.is_empty() {
            s.push_str("\nMedia:\n");
            for (kind, alt, url) in &c.media {
                if alt.is_empty() {
                    s.push_str(&format!("- {kind}: {url}\n"));
                } else {
                    s.push_str(&format!("- {kind} ({alt}): {url}\n"));
                }
            }
        }
        s.push_str(&format!("\nDiff summary: {}\n", c.diff_summary));
        if !c.paths.is_empty() {
            s.push_str("\nChanged paths:\n");
            for path in c.paths.iter().take(MAX_PROMPT_PATHS) {
                s.push_str(&format!("- {path}\n"));
            }
            if c.paths.len() > MAX_PROMPT_PATHS {
                s.push_str(&format!(
                    "- … and {} more\n",
                    c.paths.len() - MAX_PROMPT_PATHS
                ));
            }
        }
    }
    s.push_str(&format!(
        "\n## Target repo\n\n{} (default ref {})\n\n## What to do\n\n\
         Read the target repo and decide whether the upgrade would help it.\n\
         - If it does not apply, open nothing and finish with the summary \
         `not relevant: <one-line reason>`.\n\
         - If it applies, open an issue in {} that explains what to port and why; when the \
         port is clear and small, open a change request instead. Set `inspired_by_post_id` \
         to the post's id on the issue or change request. Finish with the summary \
         `opened issue #<number>: <title>` or `opened change request #<number>: <title>`.\n",
        target.full_name, target.default_ref, target.full_name
    ));
    s
}

/// The proto name of a principal kind as media URLs carry it (`human`, `agent`, `service`).
fn kind_name(k: i32) -> String {
    PrincipalKind::try_from(k)
        .unwrap_or(PrincipalKind::Human)
        .as_str_name()
        .trim_start_matches("PRINCIPAL_KIND_")
        .to_lowercase()
}

fn row(rec: &FeedTriggerRec, created_ms: i64, version: u64) -> Row {
    let t = rec.t();
    let repo = crate::community::common::ref_repo_id(&t.repo);
    Row::new(repo, rec.agent_id())
        .k1(rec.agent_id())
        .n1(created_ms)
        .version(version)
        .msg(rec)
}

impl Agents {
    /// The trigger as served: today's counters, the check interval, and its repo and sources
    /// by their current names.
    async fn feed_view(&self, rec: &FeedTriggerRec) -> AgentFeedTrigger {
        let mut t = rec.t();
        let today = utc_date(self.db.now_ms());
        let current = rec.day == today;
        t.runs_today = if current { rec.day_runs as u32 } else { 0 };
        t.spent_usd_today = if current {
            rec.day_usd_micros as f64 / 1_000_000.0
        } else {
            0.0
        };
        t.check_interval_seconds = self.config.watch_debounce.as_secs();
        let repo_id = crate::community::common::ref_repo_id(&t.repo);
        if let Ok(r) = self.forge.orgs.repo_any(&Cx::system(), &repo_id).await {
            t.repo = Some(RepoRef {
                repo: Some(repo_ref::Repo::FullName(r.full_name)),
            });
        }
        let mut sources = Vec::with_capacity(rec.source_ids.len());
        for id in &rec.source_ids {
            if let Ok(r) = self.forge.orgs.repo_any(&Cx::system(), id).await {
                sources.push(RepoRef {
                    repo: Some(repo_ref::Repo::FullName(r.full_name)),
                });
            }
        }
        t.sources = sources;
        t
    }

    /// Creates or replaces an agent's feed trigger in `repo` and makes the agent follow the
    /// sources (unfollowing those the trigger no longer names). The caller has checked the
    /// right to configure it, that the agent is enabled in the org and that it reads every
    /// source.
    pub async fn set_feed_trigger(
        &self,
        cx: &Cx,
        repo: &Repo,
        agent: &Principal,
        s: FeedSettings,
    ) -> Result<AgentFeedTrigger> {
        usd_micros(s.daily_usd_budget)?;
        if s.sources.len() > MAX_SOURCES {
            return Err(ForgeError::too_large(
                "sources",
                MAX_SOURCES as u64,
                s.sources.len() as u64,
            ));
        }
        let mut source_ids: Vec<String> = Vec::new();
        for r in &s.sources {
            if r.id == repo.id {
                return Err(ForgeError::invalid(
                    "source_is_target",
                    format!(
                        "{} cannot be a source of its own feed trigger",
                        repo.full_name
                    ),
                ));
            }
            if !source_ids.contains(&r.id) {
                source_ids.push(r.id.clone());
            }
        }
        let (rec, old_sources) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let now = tx.now_ms();
            let cur = tx
                .get_msg::<FeedTriggerRec>(&FEED_TRIGGERS, &repo.id, &agent.id)
                .await?;
            let old = cur
                .as_ref()
                .map(|(r, _)| r.source_ids.clone())
                .unwrap_or_default();
            let created = cur
                .as_ref()
                .and_then(|(r, _)| r.t().created_at)
                .unwrap_or_else(|| ts(now));
            let mut rec = cur.as_ref().map(|(r, _)| r.clone()).unwrap_or_default();
            rec.org_id = repo.org_id.clone();
            rec.configured_by = cx.actor_id.clone();
            rec.source_ids = source_ids.clone();
            rec.trigger = Some(AgentFeedTrigger {
                repo: repo_ref_id(&repo.id),
                agent: Some(agent.clone()),
                enabled: s.enabled,
                daily_usd_budget: s.daily_usd_budget,
                max_runs_per_day: s.max_runs_per_day,
                created_at: Some(created),
                updated_at: Some(ts(now)),
                ..Default::default()
            });
            let created_ms = crate::ids::ts_ms(&created) as i64;
            match &cur {
                Some((_, r)) => {
                    tx.update(&FEED_TRIGGERS, &row(&rec, created_ms, r.version))
                        .await?;
                }
                None => tx.insert(&FEED_TRIGGERS, &row(&rec, created_ms, 0)).await?,
            }
            let json = serde_json::json!({
                "agent": agent.id, "enabled": s.enabled,
                "daily_usd_budget": s.daily_usd_budget, "max_runs_per_day": s.max_runs_per_day,
                "sources": source_ids,
            })
            .to_string();
            tx.emit_event(Event {
                kind: "agent_feed_trigger.updated".into(),
                repo_id: repo.id.clone(),
                org_id: repo.org_id.clone(),
                subject: Some(SubjectRef {
                    kind: SubjectKind::Repo as i32,
                    repo: repo_ref_id(&repo.id),
                    id: agent.id.clone(),
                }),
                payload_json: json.clone(),
                ..Default::default()
            })
            .await?;
            tx.audit(
                AuditDraft::new("agent.feed_trigger_set", &repo.org_id)
                    .repo_id(&repo.id)
                    .target_id(&agent.id)
                    .after(&json),
            )
            .await?;
            tx.commit().await?;
            (rec, old)
        };
        let users = &self.forge.community.users;
        for r in &s.sources {
            users.follow_repo_as(cx, &agent.id, r).await?;
        }
        let keep: BTreeSet<&String> = source_ids.iter().collect();
        for id in old_sources.iter().filter(|id| !keep.contains(id)) {
            if let Ok(r) = self.forge.orgs.repo_any(&Cx::system(), id).await {
                users.unfollow_repo_as(cx, &agent.id, &r).await?;
            }
        }
        Ok(self.feed_view(&rec).await)
    }

    /// An agent's feed trigger in `repo`.
    pub async fn get_feed_trigger(
        &self,
        cx: &Cx,
        repo: &Repo,
        agent_id: &str,
    ) -> Result<AgentFeedTrigger> {
        let rec = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.get_msg::<FeedTriggerRec>(&FEED_TRIGGERS, &repo.id, agent_id)
                .await?
                .map(|(r, _)| r)
                .ok_or_else(|| ForgeError::not_found(format!("feed trigger of {agent_id}")))?
        };
        Ok(self.feed_view(&rec).await)
    }

    /// The feed triggers of `repo`, by agent id, after `after`.
    pub async fn list_feed_triggers(
        &self,
        cx: &Cx,
        repo: &Repo,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<AgentFeedTrigger>, String)> {
        let mut recs: Vec<FeedTriggerRec> = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<FeedTriggerRec>(
                &FEED_TRIGGERS,
                &Scan::scope(repo.id.clone(), crate::store::MAX_SCAN),
            )
            .await?
            .into_iter()
            .map(|(r, _)| r)
            .filter(|r| after.is_empty() || r.agent_id().as_str() > after)
            .collect()
        };
        recs.sort_by_key(|r| r.agent_id());
        let more = recs.len() > limit;
        recs.truncate(limit);
        let next = if more {
            recs.last().map(|r| r.agent_id()).unwrap_or_default()
        } else {
            String::new()
        };
        let mut out = Vec::with_capacity(recs.len());
        for r in &recs {
            out.push(self.feed_view(r).await);
        }
        Ok((out, next))
    }

    /// `post.published`: folds the post into the pending run of every enabled feed trigger
    /// whose agent follows the post's repo and may read it.
    pub(crate) async fn on_post_published(self: &Arc<Self>, e: &Event) -> Result<()> {
        let Some(event::Body::Post(p)) = &e.body else {
            return Ok(());
        };
        if p.state != PostState::Published as i32 || p.id.is_empty() {
            return Ok(());
        }
        let cx = Cx::system();
        let Ok(source) = self.forge.orgs.repo_any(&cx, &e.repo_id).await else {
            return Ok(());
        };
        // Who follows the repo.
        let users = &self.forge.community.users;
        let mut followers: Vec<String> = Vec::new();
        let mut token = String::new();
        loop {
            let (rows, _, next) = users.repo_followers(&cx, &source.id, &token, 1000).await?;
            followers.extend(rows.into_iter().map(|(id, _)| id));
            if next.is_empty() || followers.len() >= MAX_FOLLOWERS {
                break;
            }
            token = next;
        }
        // Their feed triggers.
        let mut triggers: Vec<FeedTriggerRec> = Vec::new();
        {
            let mut tx = self.db.begin(SURFACE, &cx).await?;
            for f in &followers {
                let scan = Scan {
                    key: Some(Key::K1(f.clone())),
                    limit: MAX_SOURCES,
                    ..Scan::default()
                };
                for (r, _) in tx
                    .scan_msgs::<FeedTriggerRec>(&FEED_TRIGGERS, &scan)
                    .await?
                {
                    let t = r.t();
                    if t.enabled && crate::community::common::ref_repo_id(&t.repo) != source.id {
                        triggers.push(r);
                    }
                }
            }
        }
        let window = self.config.watch_debounce.as_millis() as u64;
        for r in triggers {
            let agent = r.agent_id();
            let acx = Cx::new(agent.clone(), PrincipalKind::Agent).with_chain(vec![agent.clone()]);
            if !chain_reads(&self.forge.orgs, &acx, &source).await {
                continue;
            }
            let target = crate::community::common::ref_repo_id(&r.t().repo);
            let key = trigger_key(&target, &agent);
            let mut tx = self.db.begin(SURFACE, &cx).await?;
            if tx.get(&FEED_SEEN, &key, &p.id).await?.is_some() {
                continue;
            }
            let now = tx.now_ms();
            let cur = tx
                .get_msg::<FeedPendingRec>(&FEED_PENDING, "", &key)
                .await?;
            let version = cur.as_ref().map(|(_, row)| row.version);
            let Some(pending) = fold(cur.map(|(p, _)| p), &target, &agent, &p.id, now, window)
            else {
                // The pending run is full: the post waits for the next one.
                continue;
            };
            tx.insert(
                &FEED_SEEN,
                &Row::new(key.clone(), p.id.clone()).n1(now as i64),
            )
            .await?;
            let row = Row::new("", key.clone())
                .n1(pending.due_ms as i64)
                .msg(&pending);
            match version {
                Some(v) => {
                    tx.update(&FEED_PENDING, &row.version(v)).await?;
                }
                None => tx.insert(&FEED_PENDING, &row).await?,
            }
            tx.commit().await?;
        }
        Ok(())
    }

    /// Starts the ADOPT_EVALUATE runs of feed triggers whose debounce window has passed
    /// (skipping, with an event, those whose day's budget or run cap is spent).
    pub(crate) async fn fire_due_feed_triggers(self: &Arc<Self>, cx: &Cx) -> Result<usize> {
        let due = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let now = tx.now_ms() as i64;
            tx.scan_msgs::<FeedPendingRec>(&FEED_PENDING, &Scan::scope("", 100).by_n1().n1_max(now))
                .await?
        };
        let mut fired = 0;
        for (p, prow) in due {
            let key = trigger_key(&p.repo_id, &p.agent_id);
            // Claim it and charge the day.
            let (rec, budget) = {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                if tx
                    .get(&FEED_PENDING, "", &prow.id)
                    .await?
                    .map(|r| r.version)
                    != Some(prow.version)
                {
                    continue;
                }
                // Only the replica that actually removed the pending row owns this firing.
                // PostgreSQL's READ COMMITTED delete returns false after another replica
                // commits its delete; do not charge the trigger or start a duplicate run.
                if !tx.delete(&FEED_PENDING, "", &prow.id).await? {
                    tx.commit().await?;
                    continue;
                }
                let Some((mut rec, trow)) = tx
                    .get_msg::<FeedTriggerRec>(&FEED_TRIGGERS, &p.repo_id, &p.agent_id)
                    .await?
                else {
                    tx.commit().await?;
                    continue;
                };
                let t = rec.t();
                if !t.enabled {
                    tx.commit().await?;
                    continue;
                }
                roll_day(&mut rec, &utc_date(tx.now_ms()));
                let skipped = exhausted(&rec);
                let json = serde_json::json!({
                    "agent": p.agent_id, "posts": p.post_ids,
                    "skipped": skipped.unwrap_or(""),
                })
                .to_string();
                tx.emit_event(Event {
                    kind: "agent_feed_trigger.fired".into(),
                    repo_id: p.repo_id.clone(),
                    org_id: rec.org_id.clone(),
                    subject: Some(SubjectRef {
                        kind: SubjectKind::Repo as i32,
                        repo: repo_ref_id(&p.repo_id),
                        id: p.agent_id.clone(),
                    }),
                    payload_json: json.clone(),
                    ..Default::default()
                })
                .await?;
                tx.audit(
                    AuditDraft::new("agent.feed_trigger_fire", &rec.org_id)
                        .repo_id(&p.repo_id)
                        .target_id(&p.agent_id)
                        .after(&json),
                )
                .await?;
                if skipped.is_none() {
                    rec.day_runs += 1;
                }
                let created = t.created_at.as_ref().map(crate::ids::ts_ms).unwrap_or(0) as i64;
                tx.update(&FEED_TRIGGERS, &row(&rec, created, trow.version))
                    .await?;
                tx.commit().await?;
                if skipped.is_some() {
                    continue;
                }
                let limit = usd_micros(t.daily_usd_budget).unwrap_or(0);
                let budget = (limit > 0).then(|| AgentRunBudget {
                    max_usd_micros: limit.saturating_sub(rec.day_usd_micros),
                    ..AgentRunBudget::default()
                });
                (rec, budget)
            };
            let Ok(repo) = self.forge.orgs.repo_any(&Cx::system(), &p.repo_id).await else {
                continue;
            };
            let Some(first) = p.post_ids.first().cloned() else {
                continue;
            };
            if self
                .start_run(
                    cx,
                    StartRun {
                        repo,
                        task: AgentTask {
                            kind: AgentTaskKind::AdoptEvaluate as i32,
                            post_id: first.clone(),
                            ..Default::default()
                        },
                        agent_id: p.agent_id.clone(),
                        budget,
                        trigger: Trigger::Feed,
                        trigger_ref: first.clone(),
                        idempotency_key: format!("adopt:{}:{}:{first}", p.repo_id, p.agent_id),
                        chain: vec![rec.configured_by.clone()],
                        feed_trigger: key,
                        feed_post_ids: p.post_ids.clone(),
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

    /// Adds a finished feed run's cost to its trigger's daily counters.
    pub(crate) async fn charge_feed(&self, key: &str, usd: u64) -> Result<()> {
        Self::charge_feed_db(&self.db, key, usd).await
    }

    async fn charge_feed_db(db: &crate::store::Db, key: &str, usd: u64) -> Result<()> {
        if usd == 0 {
            return Ok(());
        }
        let Some((repo_id, agent_id)) = key.split_once('/') else {
            return Ok(());
        };
        for attempt in 0..8 {
            let mut tx = db.begin(SURFACE, &Cx::system()).await?;
            let current = if attempt == 0 {
                tx.get_msg::<FeedTriggerRec>(&FEED_TRIGGERS, repo_id, agent_id)
                    .await?
            } else {
                tx.get_for_update(&FEED_TRIGGERS, repo_id, agent_id)
                    .await?
                    .map(|row| row.decode().map(|rec| (rec, row)))
                    .transpose()?
            };
            let Some((mut rec, trow)) = current else {
                return Ok(());
            };
            roll_day(&mut rec, &utc_date(tx.now_ms()));
            rec.day_usd_micros = rec.day_usd_micros.saturating_add(usd);
            let created = rec
                .t()
                .created_at
                .as_ref()
                .map(crate::ids::ts_ms)
                .unwrap_or(0) as i64;
            match tx
                .update(&FEED_TRIGGERS, &row(&rec, created, trow.version))
                .await
            {
                Ok(_) => return tx.commit().await,
                Err(error)
                    if error.reason == bee_proto::v1::ErrorReason::VersionMismatch
                        && attempt < 7 =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the final retry returns its error")
    }

    /// The ADOPT_EVALUATE prompt of a run: every folded post the run's chain may read, with
    /// its source change. `INVALID_ARGUMENT`/`post_not_readable` when none is.
    pub(crate) async fn adopt_context(&self, rec: &RunRec, target: &Repo) -> Result<String> {
        let posts = self.changes.posts().ok_or_else(|| {
            crate::service::unserved(
                "ADOPT_EVALUATE runs",
                "the posts module (crate::posts::Posts::attach)",
            )
        })?;
        let run = rec.run();
        let task = run.task.clone().unwrap_or_default();
        let ids: Vec<String> = if rec.feed_post_ids.is_empty() {
            vec![task.post_id.clone()]
        } else {
            rec.feed_post_ids.clone()
        };
        let sys = Cx::system();
        let agent = run.agent.clone().unwrap_or_default();
        let acx = Cx::new(agent.id.clone(), PrincipalKind::Agent).with_chain(rec.chain.clone());
        // Media URLs are signed for the run's chain (each link re-checked when redeemed).
        let mut links = Vec::with_capacity(rec.chain.len());
        for id in &rec.chain {
            let kind = match self.forge.hooks.directory.get(id).await {
                Some(p) => kind_name(p.kind),
                None if *id == agent.id => "agent".to_string(),
                None => "human".to_string(),
            };
            links.push((id.clone(), kind));
        }
        let mut ctx = Vec::new();
        for id in &ids {
            let Ok(repo_id) = posts.locate(&sys, id).await else {
                continue;
            };
            let Ok((prec, _)) = posts.get(&sys, &repo_id, id).await else {
                continue;
            };
            if prec.state() != PostState::Published {
                continue;
            }
            let Ok(source) = self
                .forge
                .orgs
                .resolve_repo(&sys, &repo_ref_id(&repo_id).unwrap_or_default())
                .await
            else {
                continue;
            };
            if !chain_reads(&self.forge.orgs, &acx, &source).await {
                continue;
            }
            let p = prec.post().clone();
            let now = self.db.now_ms();
            let media = p
                .media
                .iter()
                .map(|m| {
                    let key = format!("{}/{}/{}", crate::posts::MEDIA_PREFIX, source.id, m.id);
                    let url = posts.uploads().signer().download_url(&key, &links, now).url;
                    let kind = if m.kind == bee_proto::v1::PostMediaKind::Video as i32 {
                        "video"
                    } else {
                        "image"
                    };
                    (kind.to_string(), m.alt_text.clone(), url)
                })
                .collect();
            let (diff, paths) = match self.changes.record(&sys, &source, &p.change_id).await {
                Ok((cr, _)) => {
                    let classes: Vec<(String, String, bool)> = cr
                        .paths
                        .iter()
                        .map(|c| (c.path.clone(), c.class.clone(), c.deleted))
                        .collect();
                    (
                        diff_summary(&classes),
                        cr.paths.iter().map(|c| c.path.clone()).collect(),
                    )
                }
                Err(_) => ("the change is not available".to_string(), Vec::new()),
            };
            ctx.push(PostContext {
                post: p,
                repo: source.full_name.clone(),
                media,
                diff_summary: diff,
                paths,
            });
        }
        if ctx.is_empty() {
            return Err(ForgeError::invalid(
                NOT_READABLE,
                format!(
                    "no post of run {} is a published post its agent can read",
                    run.id
                ),
            ));
        }
        Ok(adopt_prompt(target, &ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_round_trip_in_the_three_forms() {
        for o in [
            Outcome::NotRelevant("we never rebase".into()),
            Outcome::Issue(12, "Record usage on every run".into()),
            Outcome::ChangeRequest(7, "Back off: cap the delay".into()),
        ] {
            assert_eq!(Outcome::parse(&o.summary()), Some(o.clone()), "{o:?}");
        }
        assert_eq!(
            Outcome::NotRelevant("two\nlines".into()).summary(),
            "not relevant: two lines"
        );
        assert_eq!(
            Outcome::Issue(3, "x".into()).summary(),
            "opened issue #3: x"
        );
        assert_eq!(Outcome::parse("Committed agent/x.md"), None);
        assert_eq!(Outcome::parse("opened issue #x: y"), None);
    }

    #[test]
    fn budgets_are_checked_and_converted() {
        assert_eq!(usd_micros(0.0).unwrap(), 0);
        assert_eq!(usd_micros(1.25).unwrap(), 1_250_000);
        for bad in [-1.0, f64::NAN, f64::INFINITY, MAX_DAILY_USD * 2.0] {
            assert_eq!(usd_micros(bad).unwrap_err().subcode, "invalid_budget");
        }
    }

    fn rec(budget: f64, cap: u32, runs: u64, usd: u64) -> FeedTriggerRec {
        FeedTriggerRec {
            trigger: Some(AgentFeedTrigger {
                daily_usd_budget: budget,
                max_runs_per_day: cap,
                ..Default::default()
            }),
            day: "2026-10-28".into(),
            day_runs: runs,
            day_usd_micros: usd,
            ..Default::default()
        }
    }

    #[test]
    fn spent_days_skip_runs_and_a_new_day_resets() {
        assert_eq!(exhausted(&rec(0.0, 0, 1000, 1 << 40)), None);
        assert_eq!(exhausted(&rec(1.0, 0, 0, 999_999)), None);
        assert_eq!(
            exhausted(&rec(1.0, 0, 0, 1_000_000)),
            Some("daily budget spent")
        );
        assert_eq!(exhausted(&rec(0.0, 2, 1, 0)), None);
        assert_eq!(exhausted(&rec(0.0, 2, 2, 0)), Some("daily run cap reached"));
        let mut r = rec(1.0, 2, 2, 1_000_000);
        roll_day(&mut r, "2026-10-28");
        assert!(exhausted(&r).is_some());
        roll_day(&mut r, "2026-10-29");
        assert_eq!((r.day_runs, r.day_usd_micros), (0, 0));
        assert_eq!(exhausted(&r), None);
    }

    #[test]
    fn posts_fold_with_a_capped_trailing_debounce() {
        let w = 1000;
        let p = fold(None, "r", "a", "p1", 0, w).unwrap();
        assert_eq!(
            (p.post_ids.clone(), p.first_ms, p.due_ms),
            (vec!["p1".into()], 0, 1000)
        );
        // A second post inside the window pushes it out; a repeat changes nothing.
        let p = fold(Some(p), "r", "a", "p2", 900, w).unwrap();
        assert_eq!((p.post_ids.len(), p.due_ms), (2, 1900));
        let p = fold(Some(p), "r", "a", "p2", 1500, w).unwrap();
        assert_eq!((p.post_ids.len(), p.due_ms), (2, 1900));
        // Never later than ten windows after the first post.
        let p = fold(Some(p), "r", "a", "p3", 9_800, w).unwrap();
        assert_eq!(p.due_ms, 10_000);
        let mut p = p;
        while p.post_ids.len() < MAX_FOLDED {
            let n = p.post_ids.len();
            p = fold(Some(p), "r", "a", &format!("q{n}"), 1, w).unwrap();
        }
        assert!(fold(Some(p), "r", "a", "late", 2, w).is_none());
    }

    #[test]
    fn the_prompt_carries_the_post_change_and_target() {
        let target = Repo {
            full_name: "acme/api".into(),
            default_ref: "main".into(),
            ..Default::default()
        };
        let post = Post {
            id: "01POST".into(),
            change_number: 42,
            change_title: "Cache the build layers".into(),
            text: "Builds are 3x faster #ci".into(),
            ..Default::default()
        };
        let classes = vec![
            ("ci/build.yml".to_string(), "content".to_string(), false),
            ("ci/old.sh".to_string(), "content".to_string(), true),
        ];
        let s = adopt_prompt(
            &target,
            &[PostContext {
                post,
                repo: "lattice/engine".into(),
                media: vec![(
                    "image".into(),
                    "Before and after".into(),
                    "https://m/1".into(),
                )],
                diff_summary: diff_summary(&classes),
                paths: classes.iter().map(|c| c.0.clone()).collect(),
            }],
        );
        for want in [
            "Decide whether it applies to acme/api",
            "Post 01POST in lattice/engine",
            "Change #42: Cache the build layers",
            "Builds are 3x faster #ci",
            "- image (Before and after): https://m/1",
            "2 paths changed (1 deleted); diff classes: content 2",
            "- ci/build.yml",
            "acme/api (default ref main)",
            "inspired_by_post_id",
            "`not relevant: <one-line reason>`",
            "`opened issue #<number>: <title>`",
            "`opened change request #<number>: <title>`",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
        assert_eq!(diff_summary(&[]), "no paths changed");
    }
}

#[cfg(test)]
mod pg_race_tests {
    use super::*;
    use crate::ids::SystemClock;
    use crate::module::Registry;
    use crate::store::ForgeStore;
    use crate::store::sql::{SqlConfig, SqlStore};
    use std::time::Duration;
    use tokio::time::Instant;

    #[tokio::test]
    async fn postgres_two_replicas_fire_a_due_feed_trigger_once_when_configured() {
        let Ok(url) = std::env::var("BEE_TEST_PG") else {
            eprintln!("BEE_TEST_PG not set; skipping Postgres");
            return;
        };
        let mut cfg = SqlConfig::parse(&url).unwrap();
        if let crate::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
            *schema = format!("forge_feed_race_{}", std::process::id());
        }
        let left = Arc::new(
            SqlStore::open(cfg.clone(), Arc::new(SystemClock))
                .await
                .unwrap(),
        );
        let right = Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
        let registry = Registry::core().unwrap();
        left.migrate(&registry.migrations()).await.unwrap();

        // Exercise the exact durable claim primitive used by firing: both replicas observe the
        // pending version, but only the successful DELETE may proceed to mutate a trigger.
        for round in 0..100 {
            let id = format!("feed-claim-{}-{round}", std::process::id());
            let mut seed = left
                .begin(SURFACE, Instant::now() + Duration::from_secs(30))
                .await
                .unwrap();
            seed.insert(
                &FEED_PENDING,
                &Row::new("", id.clone()).n1(0).msg(&FeedPendingRec {
                    repo_id: "repo".into(),
                    agent_id: "agent".into(),
                    post_ids: vec![format!("post-{round}")],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
            seed.commit().await.unwrap();

            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let attempt = |store: Arc<SqlStore>, barrier: Arc<tokio::sync::Barrier>, id: String| async move {
                let mut tx = store
                    .begin(SURFACE, Instant::now() + Duration::from_secs(30))
                    .await
                    .unwrap();
                let version = tx
                    .get(&FEED_PENDING, "", &id)
                    .await
                    .unwrap()
                    .unwrap()
                    .version;
                barrier.wait().await;
                let still_current = tx
                    .get(&FEED_PENDING, "", &id)
                    .await
                    .unwrap()
                    .is_some_and(|row| row.version == version);
                let deleted = still_current && tx.delete(&FEED_PENDING, "", &id).await.unwrap();
                tx.commit().await.unwrap();
                deleted
            };
            let (a, b) = tokio::join!(
                attempt(left.clone(), barrier.clone(), id.clone()),
                attempt(right.clone(), barrier, id),
            );
            assert_ne!(
                a, b,
                "round {round}: expected exactly one pending-row claimant"
            );
        }
    }

    #[tokio::test]
    async fn postgres_concurrent_feed_charges_are_all_recorded_when_configured() {
        let Ok(url) = std::env::var("BEE_TEST_PG") else {
            eprintln!("BEE_TEST_PG not set; skipping Postgres");
            return;
        };
        let mut cfg = SqlConfig::parse(&url).unwrap();
        if let crate::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
            *schema = format!("forge_feed_charge_race_{}", std::process::id());
        }
        let left = Arc::new(
            SqlStore::open(cfg.clone(), Arc::new(SystemClock))
                .await
                .unwrap(),
        );
        let right = Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
        let registry = Registry::core().unwrap();
        left.migrate(&registry.migrations()).await.unwrap();
        let trigger = FeedTriggerRec {
            trigger: Some(AgentFeedTrigger {
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id("repo".into())),
                }),
                agent: Some(Principal {
                    id: "agent".into(),
                    ..Default::default()
                }),
                enabled: true,
                ..Default::default()
            }),
            day: "1970-01-01".into(),
            ..Default::default()
        };
        let mut seed = left
            .begin(SURFACE, Instant::now() + Duration::from_secs(30))
            .await
            .unwrap();
        seed.insert(&FEED_TRIGGERS, &row(&trigger, 0, 0))
            .await
            .unwrap();
        seed.commit().await.unwrap();

        let left_db = crate::store::Db::new(
            left.clone(),
            Arc::new(crate::ids::IdGen::new(Arc::new(SystemClock))),
        );
        let right_db = crate::store::Db::new(
            right.clone(),
            Arc::new(crate::ids::IdGen::new(Arc::new(SystemClock))),
        );
        for round in 0..20 {
            let barrier = Arc::new(tokio::sync::Barrier::new(20));
            let mut tasks = Vec::new();
            for index in 0..20 {
                let barrier = barrier.clone();
                let db = if index % 2 == 0 {
                    left_db.clone()
                } else {
                    right_db.clone()
                };
                tasks.push(tokio::spawn(async move {
                    barrier.wait().await;
                    Agents::charge_feed_db(&db, "repo/agent", 1).await.unwrap();
                }));
            }
            for task in tasks {
                task.await.unwrap();
            }
            let mut read = left_db.begin(SURFACE, &Cx::system()).await.unwrap();
            let current = read
                .get_msg::<FeedTriggerRec>(&FEED_TRIGGERS, "repo", "agent")
                .await
                .unwrap()
                .unwrap()
                .0;
            assert_eq!(current.day_usd_micros, (round + 1) * 20, "round {round}");
        }
    }
}
