//! Insights (PRODUCT §4.16, F416-014..016, F416-052): pulse, contributors, commit activity,
//! code frequency, traffic, the network graph, agent statistics and the org dependency hook.
//!
//! Everything except traffic is computed on read from the
//! [`ActivitySource`](super::ports::ActivitySource) port (history with provenance, change
//! requests, issues) and is a pure function of its data.
//!
//! # Contributors
//!
//! One entry per author principal (agents and humans alike), most changesets first. A
//! changeset whose author is an unclaimed imported identity carries a commit email; if a
//! verified account now owns that email the changeset counts for the account, otherwise the
//! entry is a *placeholder* with the email.
//!
//! # Traffic
//!
//! Views, clones, referrers and popular paths are daily counters kept for 14 days, with
//! unique visitors counted by a hashed visitor id per day. Recording is telemetry: it writes
//! no event or audit entry per hit; pruning is audited once per pass.
//!
//! | table | scope | id |
//! |---|---|---|
//! | counters | repo id | `<kind>:<day>[:<key>]`, `n1` = count |
//! | visitors | repo id | `<kind>:<day>:<key>:<visitor hash>` |
//!
//! Kinds: `v` views, `c` clones, `r` referrers (key = host), `p` paths (key = path); a
//! trailing `u` (`vu`, `cu`, ...) is the unique-visitor counter of the same kind.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_proto::v1::{
    ContributorStats, ContributorWeek, Cost, InsightsAgentStats, InsightsCodeFrequencyWeek,
    InsightsCommitWeek, InsightsOrgDependency, InsightsWindow, NetworkEdge, NetworkNode, ObjectId,
    Principal, PrincipalKind, PulseAuthorCount, PulseSummary, Repo, SubjectKind, SubjectRef,
    TrafficInterval, TrafficPoint, TrafficPopularPath, TrafficReferrer,
};

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::module::EventKind;
use crate::store::{Db, Row, Scan, TableDef};

use super::SURFACE;
use super::common::{day_of, draft, page_vec, repo_ref_id};
use super::ports::{
    ActivitySource, ChangeOutcome, DependencyQuery, DependencySource, GraphEntry, LogEntry,
};
use super::releases::Releases;
use super::users::Users;

/// Traffic counters, scoped by repo.
pub static TRAFFIC: TableDef = TableDef {
    name: "forge_traffic",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Unique-visitor markers, scoped by repo.
pub static VISITORS: TableDef = TableDef {
    name: "forge_traffic_visitors",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Insights event kinds (pruning only: insights are derived data).
pub const EVENTS: &[EventKind] = &[EventKind::new(
    "insights.traffic_pruned",
    "Old traffic counters were removed",
)];

/// Days traffic is kept.
pub const TRAFFIC_DAYS: i64 = 14;
/// Most distinct referrers or paths counted per day.
pub const MAX_KEYS_PER_DAY: usize = 100;
/// Most log entries one computation reads.
pub const MAX_LOG: usize = 50_000;
/// Default and largest network graph.
pub const DEFAULT_GRAPH: usize = 500;
/// Largest network graph.
pub const MAX_GRAPH: usize = 2000;

const DAY_MS: u64 = 86_400_000;
const WEEK_MS: u64 = 7 * DAY_MS;

/// The length of an insights window in milliseconds.
pub fn window_ms(w: InsightsWindow) -> u64 {
    match w {
        InsightsWindow::Day => DAY_MS,
        InsightsWindow::ThreeDays => 3 * DAY_MS,
        InsightsWindow::Month => 30 * DAY_MS,
        InsightsWindow::Quarter => 90 * DAY_MS,
        InsightsWindow::Year => 365 * DAY_MS,
        InsightsWindow::Week | InsightsWindow::Unspecified => WEEK_MS,
    }
}

fn sunday_of(ms: u64) -> u64 {
    let days = ms / DAY_MS;
    let dow = (days + 4) % 7; // 0 = Sunday
    (days - dow) * DAY_MS
}

fn visitor_hash(v: &str) -> String {
    hex(&blake3::hash(v.as_bytes()).as_bytes()[..8])
}

fn is_agent(p: &Principal) -> bool {
    p.kind == PrincipalKind::Agent as i32
}

fn agent_slot<'a>(
    by: &'a mut BTreeMap<String, (InsightsAgentStats, Vec<u32>)>,
    p: &Principal,
) -> &'a mut (InsightsAgentStats, Vec<u32>) {
    by.entry(p.id.clone()).or_insert_with(|| {
        (
            InsightsAgentStats {
                agent: Some(p.clone()),
                cost: Some(Cost::default()),
                ..Default::default()
            },
            Vec::new(),
        )
    })
}

/// Insights over the store and ports.
pub struct Insights {
    db: Db,
    activity: Arc<dyn ActivitySource>,
    deps: Arc<dyn DependencySource>,
    users: Arc<Users>,
    releases: Arc<Releases>,
}

/// Contributor filters.
#[derive(Clone, Debug, Default)]
pub struct ContributorQuery {
    /// Ref whose history is counted (empty = default ref).
    pub ref_name: String,
    /// Principal kind filter (`Unspecified` = all).
    pub kind: i32,
    /// Window start (ms, 0 = beginning).
    pub since_ms: u64,
    /// Window end (ms, 0 = now).
    pub until_ms: u64,
    /// Include weekly series.
    pub include_weeks: bool,
}

/// A contributors page with the totals.
#[derive(Clone, Debug, Default)]
pub struct Contributors {
    /// The page.
    pub page: Vec<ContributorStats>,
    /// Humans in the whole result.
    pub humans: u32,
    /// Agents in the whole result.
    pub agents: u32,
    /// Placeholders in the whole result.
    pub placeholders: u32,
    /// Next page token.
    pub next: String,
}

impl Insights {
    /// Insights over `db` and the ports.
    pub fn new(
        db: Db,
        activity: Arc<dyn ActivitySource>,
        deps: Arc<dyn DependencySource>,
        users: Arc<Users>,
        releases: Arc<Releases>,
    ) -> Insights {
        Insights {
            db,
            activity,
            deps,
            users,
            releases,
        }
    }

    fn range(&self, w: InsightsWindow) -> (u64, u64) {
        let now = self.db.now_ms();
        (now.saturating_sub(window_ms(w)), now)
    }

    async fn log(
        &self,
        repo: &Repo,
        ref_name: &str,
        since: u64,
        until: u64,
    ) -> Result<Vec<LogEntry>> {
        self.activity
            .log(repo, ref_name, since, until, MAX_LOG)
            .await
    }

    // ---- pulse ------------------------------------------------------------------------

    /// The pulse of a repo over a window.
    pub async fn pulse(
        &self,
        cx: &Cx,
        repo: &Repo,
        window: InsightsWindow,
    ) -> Result<PulseSummary> {
        let (from, to) = self.range(window);
        let log = self.log(repo, "", from, to).await?;
        let crs = self.activity.change_requests(repo, from, to).await?;
        let issues = self.activity.issues(repo, from, to).await?;
        let inside = |t: u64| t >= from && t < to;
        let mut landed: Vec<&super::ports::ChangeActivity> = crs
            .iter()
            .filter(|c| c.outcome == ChangeOutcome::Landed && inside(c.ended_ms))
            .collect();
        landed.sort_by_key(|c| std::cmp::Reverse((c.ended_ms, c.number)));
        let mut opened_issues: Vec<&super::ports::IssueActivity> =
            issues.iter().filter(|i| inside(i.opened_ms)).collect();
        opened_issues.sort_by_key(|i| std::cmp::Reverse((i.opened_ms, i.number)));
        let mut by_author: BTreeMap<String, (Principal, u32)> = BTreeMap::new();
        let (mut adds, mut dels, mut files) = (0u64, 0u64, 0u32);
        for e in &log {
            adds += e.additions;
            dels += e.deletions;
            files += e.files_changed;
            let key = if e.author.id.is_empty() {
                e.placeholder_email.clone()
            } else {
                e.author.id.clone()
            };
            by_author
                .entry(key)
                .or_insert_with(|| (e.author.clone(), 0))
                .1 += 1;
        }
        let mut top: Vec<(String, Principal, u32)> = by_author
            .iter()
            .map(|(k, (p, n))| (k.clone(), p.clone(), *n))
            .collect();
        top.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        Ok(PulseSummary {
            repo: repo_ref_id(&repo.id),
            from_at: Some(ts(from)),
            to_at: Some(ts(to)),
            change_requests_opened: crs.iter().filter(|c| inside(c.opened_ms)).count() as u32,
            change_requests_landed: landed.len() as u32,
            change_requests_closed: crs
                .iter()
                .filter(|c| c.outcome == ChangeOutcome::Closed && inside(c.ended_ms))
                .count() as u32,
            issues_opened: opened_issues.len() as u32,
            issues_closed: issues
                .iter()
                .filter(|i| i.closed_ms > 0 && inside(i.closed_ms))
                .count() as u32,
            changesets_landed: log.len() as u32,
            files_changed: files,
            additions: adds,
            deletions: dels,
            human_authors: by_author.values().filter(|(p, _)| !is_agent(p)).count() as u32,
            agent_authors: by_author.values().filter(|(p, _)| is_agent(p)).count() as u32,
            releases_published: self
                .releases
                .published_between(cx, &repo.id, from, to)
                .await?,
            landed_change_requests: landed
                .iter()
                .take(20)
                .map(|c| SubjectRef {
                    kind: SubjectKind::ChangeRequest as i32,
                    repo: repo_ref_id(&repo.id),
                    id: c.number.to_string(),
                })
                .collect(),
            opened_issues: opened_issues
                .iter()
                .take(20)
                .map(|i| SubjectRef {
                    kind: SubjectKind::Issue as i32,
                    repo: repo_ref_id(&repo.id),
                    id: i.number.to_string(),
                })
                .collect(),
            top_authors: top
                .into_iter()
                .take(10)
                .map(|(_, p, n)| PulseAuthorCount {
                    author: Some(p),
                    changesets: n,
                })
                .collect(),
        })
    }

    // ---- contributors -----------------------------------------------------------------

    /// Contributors with changeset and line counts, most changesets first.
    pub async fn contributors(
        &self,
        cx: &Cx,
        repo: &Repo,
        q: &ContributorQuery,
        token: &str,
        limit: usize,
    ) -> Result<Contributors> {
        let until = if q.until_ms == 0 {
            self.db.now_ms() + 1
        } else {
            q.until_ms
        };
        let log = self.log(repo, &q.ref_name, q.since_ms, until).await?;
        let mut emails: Vec<String> = log
            .iter()
            .filter(|e| e.author.id.is_empty() && !e.placeholder_email.is_empty())
            .map(|e| e.placeholder_email.clone())
            .collect();
        emails.sort();
        emails.dedup();
        let mut claimed: BTreeMap<String, Principal> = BTreeMap::new();
        for chunk in emails.chunks(100) {
            for m in self.users.resolve_commit_emails(cx, chunk).await? {
                if let Some(p) = m.principal
                    && !m.placeholder
                {
                    claimed.insert(m.email.to_lowercase(), p);
                }
            }
        }
        struct Acc {
            stats: ContributorStats,
            weeks: BTreeMap<u64, ContributorWeek>,
        }
        let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
        for e in &log {
            let (key, principal, placeholder, email) = if !e.author.id.is_empty() {
                (e.author.id.clone(), e.author.clone(), false, String::new())
            } else if let Some(p) = claimed.get(&e.placeholder_email.to_lowercase()) {
                (p.id.clone(), p.clone(), false, String::new())
            } else {
                (
                    format!("email:{}", e.placeholder_email.to_lowercase()),
                    Principal::default(),
                    true,
                    e.placeholder_email.clone(),
                )
            };
            if q.kind != 0 && !placeholder && principal.kind != q.kind {
                continue;
            }
            if q.kind != 0 && placeholder {
                continue;
            }
            let a = acc.entry(key).or_insert_with(|| Acc {
                stats: ContributorStats {
                    principal: (!placeholder).then(|| principal.clone()),
                    placeholder,
                    placeholder_email: email,
                    ..Default::default()
                },
                weeks: BTreeMap::new(),
            });
            a.stats.changesets += 1;
            a.stats.additions += e.additions;
            a.stats.deletions += e.deletions;
            let t = e.time_ms;
            if a.stats
                .first_at
                .as_ref()
                .is_none_or(|f| crate::ids::ts_ms(f) > t)
            {
                a.stats.first_at = Some(ts(t));
            }
            if a.stats
                .last_at
                .as_ref()
                .is_none_or(|l| crate::ids::ts_ms(l) < t)
            {
                a.stats.last_at = Some(ts(t));
            }
            if q.include_weeks {
                let ws = sunday_of(t);
                let w = a.weeks.entry(ws).or_insert_with(|| ContributorWeek {
                    week_start: Some(ts(ws)),
                    ..Default::default()
                });
                w.changesets += 1;
                w.additions += e.additions;
                w.deletions += e.deletions;
            }
        }
        let mut all: Vec<(String, ContributorStats)> = acc
            .into_iter()
            .map(|(k, mut a)| {
                a.stats.weeks = a.weeks.into_values().collect();
                (k, a.stats)
            })
            .collect();
        all.sort_by(|a, b| b.1.changesets.cmp(&a.1.changesets).then(a.0.cmp(&b.0)));
        let humans = all
            .iter()
            .filter(|(_, s)| !s.placeholder && s.principal.as_ref().is_some_and(|p| !is_agent(p)))
            .count() as u32;
        let agents = all
            .iter()
            .filter(|(_, s)| s.principal.as_ref().is_some_and(is_agent))
            .count() as u32;
        let placeholders = all.iter().filter(|(_, s)| s.placeholder).count() as u32;
        let (page, next) = page_vec(all.into_iter().map(|(_, s)| s).collect(), token, limit)?;
        Ok(Contributors {
            page,
            humans,
            agents,
            placeholders,
            next,
        })
    }

    // ---- activity series --------------------------------------------------------------

    /// Commits per day for the last 52 weeks, oldest week first.
    pub async fn commit_activity(&self, repo: &Repo) -> Result<Vec<InsightsCommitWeek>> {
        let now = self.db.now_ms();
        let this_week = sunday_of(now);
        let first = this_week - 51 * WEEK_MS;
        let log = self.log(repo, "", first, now + 1).await?;
        let mut weeks: Vec<InsightsCommitWeek> = (0..52)
            .map(|i| InsightsCommitWeek {
                week_start: Some(ts(first + i * WEEK_MS)),
                days: vec![0; 7],
                total: 0,
            })
            .collect();
        for e in log {
            let idx = ((sunday_of(e.time_ms) - first) / WEEK_MS) as usize;
            let dow = ((e.time_ms / DAY_MS + 4) % 7) as usize;
            if let Some(w) = weeks.get_mut(idx) {
                w.days[dow] += 1;
                w.total += 1;
            }
        }
        Ok(weeks)
    }

    /// Additions and deletions per week over the repo's history (most recent 520 weeks).
    pub async fn code_frequency(&self, repo: &Repo) -> Result<Vec<InsightsCodeFrequencyWeek>> {
        let now = self.db.now_ms();
        let log = self.log(repo, "", 0, now + 1).await?;
        let Some(first) = log.iter().map(|e| sunday_of(e.time_ms)).min() else {
            return Ok(Vec::new());
        };
        let last = sunday_of(now);
        let first = first.max(last.saturating_sub(519 * WEEK_MS));
        let n = ((last - first) / WEEK_MS + 1) as usize;
        let mut weeks: Vec<InsightsCodeFrequencyWeek> = (0..n as u64)
            .map(|i| InsightsCodeFrequencyWeek {
                week_start: Some(ts(first + i * WEEK_MS)),
                additions: 0,
                deletions: 0,
            })
            .collect();
        for e in log {
            let ws = sunday_of(e.time_ms);
            if ws < first {
                continue;
            }
            let w = &mut weeks[((ws - first) / WEEK_MS) as usize];
            w.additions += e.additions;
            w.deletions += e.deletions;
        }
        Ok(weeks)
    }

    // ---- network graph ----------------------------------------------------------------

    /// Changesets across refs (and forks) with their parent edges, newest first. Lanes are a
    /// layout hint: a node continues the lane of its first parent when that lane is free,
    /// otherwise it opens a new one.
    #[allow(clippy::too_many_arguments)]
    pub async fn network(
        &self,
        repo: &Repo,
        include_forks: bool,
        refs: &[String],
        since_ms: u64,
        until_ms: u64,
        max_nodes: usize,
    ) -> Result<(Vec<NetworkNode>, Vec<NetworkEdge>, bool)> {
        let cap = if max_nodes == 0 {
            DEFAULT_GRAPH
        } else {
            max_nodes.min(MAX_GRAPH)
        };
        let until = if until_ms == 0 {
            self.db.now_ms() + 1
        } else {
            until_ms
        };
        let mut entries: Vec<GraphEntry> = self
            .activity
            .graph(repo, include_forks, refs, since_ms, until, cap + 1)
            .await?;
        let truncated = entries.len() > cap;
        entries.truncate(cap);
        let ids: BTreeSet<Vec<u8>> = entries
            .iter()
            .map(|g| g.entry.changeset.raw.clone())
            .collect();
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by_key(|i| {
            (
                entries[*i].entry.time_ms,
                entries[*i].entry.changeset.raw.clone(),
            )
        });
        let mut lane_of: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
        let mut taken: BTreeSet<(Vec<u8>, u32)> = BTreeSet::new();
        let mut next_lane = 0u32;
        for i in &order {
            let e = &entries[*i].entry;
            let parent_lane = e
                .parents
                .first()
                .and_then(|p| lane_of.get(&p.raw).map(|l| (p.raw.clone(), *l)));
            let lane = match parent_lane {
                Some(key) if !taken.contains(&key) => {
                    let l = key.1;
                    taken.insert(key);
                    l
                }
                _ => {
                    let l = next_lane;
                    next_lane += 1;
                    l
                }
            };
            lane_of.insert(e.changeset.raw.clone(), lane);
        }
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for g in &entries {
            let e = &g.entry;
            nodes.push(NetworkNode {
                changeset: Some(e.changeset.clone()),
                repo: repo_ref_id(&g.repo_id),
                repo_full_name: g.repo_full_name.clone(),
                refs: e.refs.clone(),
                author: Some(e.author.clone()),
                summary: e.summary.lines().next().unwrap_or("").to_string(),
                committed_at: Some(ts(e.time_ms)),
                lane: lane_of.get(&e.changeset.raw).copied().unwrap_or(0),
            });
            for p in &e.parents {
                if ids.contains(&p.raw) {
                    edges.push(NetworkEdge {
                        child: Some(e.changeset.clone()),
                        parent: Some(p.clone()),
                    });
                }
            }
        }
        Ok((nodes, edges, truncated))
    }

    // ---- agents and dependencies ------------------------------------------------------

    /// Per-agent statistics over a window, most landed changesets first.
    pub async fn agent_stats(
        &self,
        repo: &Repo,
        window: InsightsWindow,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<InsightsAgentStats>, Cost, String)> {
        let (from, to) = self.range(window);
        let log = self.log(repo, "", from, to).await?;
        let crs = self.activity.change_requests(repo, from, to).await?;
        let mut by: BTreeMap<String, (InsightsAgentStats, Vec<u32>)> = BTreeMap::new();
        for e in &log {
            if !is_agent(&e.author) {
                continue;
            }
            let (s, _) = agent_slot(&mut by, &e.author);
            s.changesets_landed += 1;
            if e.reverted {
                s.changesets_reverted += 1;
            }
            s.additions += e.additions;
            s.deletions += e.deletions;
            let c = s.cost.get_or_insert_with(Cost::default);
            c.input_tokens += e.cost.0;
            c.output_tokens += e.cost.1;
            c.usd_micros += e.cost.2;
        }
        for c in &crs {
            if !is_agent(&c.author) {
                continue;
            }
            let (s, rounds) = agent_slot(&mut by, &c.author);
            if c.opened_ms >= from && c.opened_ms < to {
                s.change_requests_opened += 1;
            }
            match c.outcome {
                ChangeOutcome::Landed if c.ended_ms >= from && c.ended_ms < to => {
                    s.change_requests_landed += 1
                }
                ChangeOutcome::Closed if c.ended_ms >= from && c.ended_ms < to => {
                    s.change_requests_closed += 1
                }
                _ => {}
            }
            rounds.push(c.review_rounds);
        }
        let mut total = Cost::default();
        let mut out: Vec<InsightsAgentStats> = by
            .into_values()
            .map(|(mut s, mut rounds)| {
                s.revert_rate = if s.changesets_landed == 0 {
                    0.0
                } else {
                    f64::from(s.changesets_reverted) / f64::from(s.changesets_landed)
                };
                rounds.sort_unstable();
                s.review_rounds_median = rounds.get(rounds.len() / 2).copied().unwrap_or(0);
                if let Some(c) = &s.cost {
                    total.input_tokens += c.input_tokens;
                    total.output_tokens += c.output_tokens;
                    total.usd_micros += c.usd_micros;
                }
                s
            })
            .collect();
        out.sort_by(|a, b| {
            b.changesets_landed.cmp(&a.changesets_landed).then(
                a.agent
                    .as_ref()
                    .map(|p| &p.id)
                    .cmp(&b.agent.as_ref().map(|p| &p.id)),
            )
        });
        let (page, next) = page_vec(out, token, limit)?;
        Ok((page, total, next))
    }

    /// Org-wide dependencies from the dependency port, sorted by package, with the total.
    pub async fn org_dependencies(
        &self,
        org_id: &str,
        q: &DependencyQuery,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<InsightsOrgDependency>, u64, String)> {
        let mut all = self.deps.org_dependencies(org_id, q).await?;
        all.sort_by(|a, b| {
            a.ecosystem
                .cmp(&b.ecosystem)
                .then(a.package_name.cmp(&b.package_name))
        });
        let total = all.len() as u64;
        let (page, next) = page_vec(all, token, limit)?;
        Ok((page, total, next))
    }

    // ---- traffic ----------------------------------------------------------------------

    async fn bump(
        tx: &mut crate::store::Tx,
        repo_id: &str,
        kind: &str,
        day: i64,
        key: &str,
        visitor: &str,
    ) -> Result<()> {
        let suffix = if key.is_empty() {
            String::new()
        } else {
            format!(":{key}")
        };
        if !key.is_empty() {
            let marker = format!("{kind}:{day:08}{suffix}");
            let existing = tx.get(&TRAFFIC, repo_id, &marker).await?;
            if existing.is_none() {
                let distinct = tx
                    .scan(&TRAFFIC, &Scan::scope(repo_id, crate::store::MAX_SCAN))
                    .await?
                    .iter()
                    .filter(|r| r.id.starts_with(&format!("{kind}:{day:08}:")))
                    .count();
                if distinct >= MAX_KEYS_PER_DAY {
                    return Ok(());
                }
            }
        }
        tx.add_n1(&TRAFFIC, repo_id, &format!("{kind}:{day:08}{suffix}"), 1)
            .await?;
        let vid = format!("{kind}:{day:08}{suffix}:{}", visitor_hash(visitor));
        if tx.get(&VISITORS, repo_id, &vid).await?.is_none() {
            tx.insert(&VISITORS, &Row::new(repo_id, vid)).await?;
            tx.add_n1(&TRAFFIC, repo_id, &format!("{kind}u:{day:08}{suffix}"), 1)
                .await?;
        }
        Ok(())
    }

    /// Records a page view (and its referrer host and path).
    pub async fn record_view(
        &self,
        repo_id: &str,
        visitor: &str,
        path: &str,
        referrer: &str,
    ) -> Result<()> {
        let cx = Cx::system();
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        let day = day_of(tx.now_ms());
        Self::bump(&mut tx, repo_id, "v", day, "", visitor).await?;
        if !path.is_empty() && path.len() <= 255 {
            Self::bump(&mut tx, repo_id, "p", day, path, visitor).await?;
        }
        let host = referrer
            .split("://")
            .nth(1)
            .unwrap_or(referrer)
            .split('/')
            .next()
            .unwrap_or("")
            .to_lowercase();
        if !host.is_empty() && host.len() <= 255 {
            Self::bump(&mut tx, repo_id, "r", day, &host, visitor).await?;
        }
        tx.commit().await
    }

    /// Records a clone or fetch.
    pub async fn record_clone(&self, repo_id: &str, visitor: &str) -> Result<()> {
        let cx = Cx::system();
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        let day = day_of(tx.now_ms());
        Self::bump(&mut tx, repo_id, "c", day, "", visitor).await?;
        tx.commit().await
    }

    async fn counters(&self, cx: &Cx, repo_id: &str) -> Result<BTreeMap<String, i64>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan(&TRAFFIC, &Scan::scope(repo_id, crate::store::MAX_SCAN))
            .await?
            .into_iter()
            .map(|r| (r.id, r.n1.unwrap_or(0)))
            .collect())
    }

    /// Views or clones over the last 14 days: totals (the unique total counts distinct
    /// visitors across the window) and one point per day or per week.
    pub async fn traffic(
        &self,
        cx: &Cx,
        repo_id: &str,
        clones: bool,
        interval: TrafficInterval,
    ) -> Result<(u64, u64, Vec<TrafficPoint>)> {
        let kind = if clones { "c" } else { "v" };
        let today = day_of(self.db.now_ms());
        let from = today - (TRAFFIC_DAYS - 1);
        let c = self.counters(cx, repo_id).await?;
        let mut points = Vec::new();
        let (mut count, mut uniq) = (0u64, 0u64);
        for d in from..=today {
            let n = *c.get(&format!("{kind}:{d:08}")).unwrap_or(&0) as u64;
            let u = *c.get(&format!("{kind}u:{d:08}")).unwrap_or(&0) as u64;
            count += n;
            uniq += u;
            points.push(TrafficPoint {
                at: Some(ts(d as u64 * DAY_MS)),
                count: n,
                uniques: u,
            });
        }
        let mut distinct = BTreeSet::new();
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            for r in tx
                .scan(&VISITORS, &Scan::scope(repo_id, crate::store::MAX_SCAN))
                .await?
            {
                let mut parts = r.id.split(':');
                if parts.next() == Some(kind)
                    && parts
                        .next()
                        .and_then(|d| d.parse::<i64>().ok())
                        .is_some_and(|d| d >= from)
                {
                    distinct.insert(r.id.rsplit(':').next().unwrap_or("").to_string());
                }
            }
        }
        let uniques = if distinct.is_empty() {
            uniq
        } else {
            distinct.len() as u64
        };
        if interval == TrafficInterval::Week {
            let mut weeks: BTreeMap<u64, TrafficPoint> = BTreeMap::new();
            for p in points {
                let at = p.at.as_ref().map(crate::ids::ts_ms).unwrap_or(0);
                let ws = sunday_of(at);
                let w = weeks.entry(ws).or_insert_with(|| TrafficPoint {
                    at: Some(ts(ws)),
                    ..Default::default()
                });
                w.count += p.count;
                w.uniques += p.uniques;
            }
            points = weeks.into_values().collect();
        }
        Ok((count, uniques, points))
    }

    /// Top 10 referrers over the last 14 days.
    pub async fn referrers(&self, cx: &Cx, repo_id: &str) -> Result<Vec<TrafficReferrer>> {
        let rows = self.keyed(cx, repo_id, "r").await?;
        Ok(rows
            .into_iter()
            .take(10)
            .map(|(k, n, u)| TrafficReferrer {
                referrer: k,
                count: n,
                uniques: u,
            })
            .collect())
    }

    /// Top 10 paths over the last 14 days.
    pub async fn popular_paths(&self, cx: &Cx, repo_id: &str) -> Result<Vec<TrafficPopularPath>> {
        let rows = self.keyed(cx, repo_id, "p").await?;
        Ok(rows
            .into_iter()
            .take(10)
            .map(|(k, n, u)| TrafficPopularPath {
                title: k.clone(),
                path: k,
                count: n,
                uniques: u,
            })
            .collect())
    }

    async fn keyed(&self, cx: &Cx, repo_id: &str, kind: &str) -> Result<Vec<(String, u64, u64)>> {
        let from = day_of(self.db.now_ms()) - (TRAFFIC_DAYS - 1);
        let c = self.counters(cx, repo_id).await?;
        let mut sums: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for (id, n) in &c {
            let mut parts = id.splitn(3, ':');
            let (k, day, key) = (
                parts.next().unwrap_or(""),
                parts.next().and_then(|d| d.parse::<i64>().ok()),
                parts.next(),
            );
            if let (Some(day), Some(key)) = (day, key)
                && day >= from
            {
                if k == kind {
                    sums.entry(key.to_string()).or_default().0 += *n as u64;
                } else if k == format!("{kind}u") {
                    sums.entry(key.to_string()).or_default().1 += *n as u64;
                }
            }
        }
        let mut v: Vec<(String, u64, u64)> =
            sums.into_iter().map(|(k, (n, u))| (k, n, u)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
        Ok(v)
    }

    /// Removes traffic older than 14 days; returns the rows removed.
    pub async fn prune_traffic(&self, cx: &Cx) -> Result<usize> {
        let cutoff = day_of(self.db.now_ms()) - TRAFFIC_DAYS;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut n = 0;
        for t in [&TRAFFIC, &VISITORS] {
            for r in tx
                .scan(
                    t,
                    &Scan {
                        limit: crate::store::MAX_SCAN,
                        ..Scan::default()
                    },
                )
                .await?
            {
                let day = r.id.split(':').nth(1).and_then(|d| d.parse::<i64>().ok());
                if day.is_some_and(|d| d < cutoff) {
                    tx.delete(t, &r.scope, &r.id).await?;
                    n += 1;
                }
            }
        }
        if n > 0 {
            tx.audit(draft(
                "insights.traffic_prune",
                "",
                "",
                SubjectKind::Unspecified,
                "traffic",
                &serde_json::json!({"rows": n}),
            ))
            .await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// A tree id helper for callers that cache analysis by tree.
    pub fn cache_key(tree: &ObjectId) -> String {
        hex(&tree.raw)
    }

    /// Ensures the window is a known value.
    pub fn check_window(w: i32) -> Result<InsightsWindow> {
        InsightsWindow::try_from(w)
            .map_err(|_| ForgeError::invalid("invalid_window", "unknown insights window"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weeks_start_on_sunday() {
        // 2026-10-18 is a Sunday: the week starts at its midnight.
        let sunday = super::super::tz::days(2026, 10, 18) as u64 * DAY_MS;
        assert_eq!(sunday_of(sunday + 5 * 3_600_000), sunday);
        assert_eq!(sunday_of(sunday + 6 * DAY_MS + 1000), sunday);
        assert_eq!(sunday_of(sunday + 7 * DAY_MS), sunday + WEEK_MS);
        assert_eq!(window_ms(InsightsWindow::Unspecified), WEEK_MS);
    }
}
