//! The activity half of `UserService`: contribution graph, activity overview, home feed and
//! achievements, over [`crate::community::activity`]. Every answer leaves out rows of repos
//! the caller cannot read; the graph counts them anonymously only when their owner turned on
//! `show_private_contributions`.

use std::collections::{BTreeMap, BTreeSet};

use bee_policy::Action;
use bee_proto::v1::{
    GetUserActivityOverviewRequest, GetUserContributionGraphRequest, ListUserAchievementsRequest,
    ListUserAchievementsResponse, ListUserFeedEventsRequest, ListUserFeedEventsResponse,
    UserAchievement, UserActivityOverview, UserContributionDay, UserContributionGraph,
    UserFeedEvent,
};

use crate::auth::Caller;
use crate::community::activity::{
    self, ActivityRec, CONTRIBUTION_CLASSES, TIERS, feed_cursor, feed_token,
};
use crate::community::common::principal_ref;
use crate::community::tz::{Zone, date_string, parse_date};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::service::{ForgeServices, next, page};

/// Longest graph range in days.
pub const MAX_GRAPH_DAYS: i64 = 371;
/// Most followed users and starred repos a feed merges.
pub const MAX_FEED_SOURCES: usize = 1000;
/// Entries of the overview's top lists.
const TOP: usize = 5;

const DAY_MS: i64 = 86_400_000;

/// Which repos the caller may read, memoized per call.
struct Visible<'a> {
    svc: &'a ForgeServices,
    caller: &'a Caller,
    cx: &'a Cx,
    seen: BTreeMap<String, Option<(String, String)>>,
}

impl Visible<'_> {
    /// `(repo full name, org id)` when the caller can read the repo.
    async fn repo(&mut self, repo_id: &str) -> Option<(String, String)> {
        if let Some(v) = self.seen.get(repo_id) {
            return v.clone();
        }
        let v = match self.svc.fg().orgs.repo_any(self.cx, repo_id).await {
            Ok(repo) if repo.deleted_at.is_none() => self
                .svc
                .fg()
                .authz
                .repo(self.caller, &repo, Action::Read, &[], None, None)
                .await
                .ok()
                .map(|_| (repo.full_name.clone(), repo.org_id.clone())),
            _ => None,
        };
        self.seen.insert(repo_id.to_string(), v.clone());
        v
    }

    /// Whether the caller may see `rec`: rows without a repo are public (follows).
    async fn sees(&mut self, rec: &ActivityRec) -> bool {
        rec.repo_id.is_empty() || self.repo(&rec.repo_id).await.is_some()
    }
}

/// A date range `[from, to]` (day numbers) and the zone it is in.
fn range(
    from: &str,
    to: &str,
    zone: &str,
    now_ms: u64,
    default_days: i64,
) -> Result<(i64, i64, Zone)> {
    let zone = Zone::parse(if zone.is_empty() { "UTC" } else { zone })?;
    let bad = |f: &str| ForgeError::invalid("invalid_date", format!("{f} must be YYYY-MM-DD"));
    let to_day = if to.is_empty() {
        zone.to_local(now_ms as i64).div_euclid(DAY_MS)
    } else {
        parse_date(to).ok_or_else(|| bad("to_date"))?
    };
    let from_day = if from.is_empty() {
        to_day - default_days + 1
    } else {
        parse_date(from).ok_or_else(|| bad("from_date"))?
    };
    if from_day > to_day {
        return Err(ForgeError::invalid(
            "invalid_range",
            "from_date is after to_date",
        ));
    }
    if to_day - from_day + 1 > MAX_GRAPH_DAYS {
        return Err(ForgeError::invalid(
            "invalid_range",
            format!("a range covers at most {MAX_GRAPH_DAYS} days"),
        ));
    }
    Ok((from_day, to_day, zone))
}

/// UTC bounds `[start, end)` (ms) of local days `from..=to`.
fn bounds(zone: &Zone, from: i64, to: i64) -> (u64, u64) {
    let start = zone.to_utc(from * DAY_MS).max(0) as u64;
    let end = zone.to_utc((to + 1) * DAY_MS).max(0) as u64;
    (start, end)
}

impl ForgeServices {
    async fn org_filter(&self, cx: &Cx, org: &str) -> Result<Option<String>> {
        if org.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.fg().orgs.get_org(cx, org).await?.id))
    }

    /// `GetUserContributionGraph`.
    pub(crate) async fn contribution_graph(
        &self,
        caller: &Caller,
        r: GetUserContributionGraphRequest,
    ) -> Result<UserContributionGraph> {
        let cx = self.user_cx(caller, Action::Read)?;
        let pid = self.uid(&cx, &r.login).await?;
        let (profile, _) = self.community().users.ensure(&cx, &pid).await?;
        let (from, to, zone) = range(
            &r.from_date,
            &r.to_date,
            &r.time_zone,
            self.fg().db.now_ms(),
            365,
        )?;
        let org = self.org_filter(&cx, &r.org).await?;
        let private_ok = pid == caller.cx.actor_id
            || profile
                .settings
                .as_ref()
                .is_some_and(|s| s.show_private_contributions);
        let (start, end) = bounds(&zone, from, to);
        let rows = self
            .community()
            .activity
            .range(&cx, &activity::user_scope(&pid), start, end)
            .await?;
        let mut vis = Visible {
            svc: self,
            caller,
            cx: &cx,
            seen: BTreeMap::new(),
        };
        let mut counts: BTreeMap<i64, u32> = BTreeMap::new();
        let mut includes_private = false;
        for rec in rows {
            if !CONTRIBUTION_CLASSES.contains(&rec.class.as_str())
                || org.as_ref().is_some_and(|o| *o != rec.org_id)
            {
                continue;
            }
            if !vis.sees(&rec).await {
                if !private_ok {
                    continue;
                }
                includes_private = true;
            }
            let day = zone.to_local(rec.time_ms as i64).div_euclid(DAY_MS);
            *counts.entry(day).or_default() += 1;
        }
        let max = counts.values().copied().max().unwrap_or(0);
        let days = (from..=to)
            .map(|d| {
                let count = counts.get(&d).copied().unwrap_or(0);
                UserContributionDay {
                    date: date_string(d),
                    count,
                    level: activity::level(count, max),
                }
            })
            .collect();
        Ok(UserContributionGraph {
            login: profile
                .profile
                .as_ref()
                .map(|p| p.login.clone())
                .unwrap_or_default(),
            from_date: date_string(from),
            to_date: date_string(to),
            total: counts.values().sum(),
            days,
            includes_private,
        })
    }

    /// `GetUserActivityOverview`.
    pub(crate) async fn activity_overview(
        &self,
        caller: &Caller,
        r: GetUserActivityOverviewRequest,
    ) -> Result<UserActivityOverview> {
        let cx = self.user_cx(caller, Action::Read)?;
        let pid = self.uid(&cx, &r.login).await?;
        let (profile, _) = self.community().users.ensure(&cx, &pid).await?;
        let (from, to, zone) = range(&r.from_date, &r.to_date, "", self.fg().db.now_ms(), 365)?;
        let org = self.org_filter(&cx, &r.org).await?;
        let (start, end) = bounds(&zone, from, to);
        let rows = self
            .community()
            .activity
            .range(&cx, &activity::user_scope(&pid), start, end)
            .await?;
        let mut vis = Visible {
            svc: self,
            caller,
            cx: &cx,
            seen: BTreeMap::new(),
        };
        let mut by_class: BTreeMap<String, u64> = BTreeMap::new();
        let mut repos: BTreeMap<String, u64> = BTreeMap::new();
        let mut orgs: BTreeMap<String, u64> = BTreeMap::new();
        for rec in rows {
            if !CONTRIBUTION_CLASSES.contains(&rec.class.as_str())
                || org.as_ref().is_some_and(|o| *o != rec.org_id)
            {
                continue;
            }
            let Some((name, org_id)) = vis.repo(&rec.repo_id).await else {
                continue;
            };
            *by_class.entry(rec.class.clone()).or_default() += 1;
            *repos.entry(name).or_default() += 1;
            *orgs.entry(org_id).or_default() += 1;
        }
        let total: u64 = by_class.values().sum();
        let pct = |c: &str| {
            if total == 0 {
                0.0
            } else {
                by_class.get(c).copied().unwrap_or(0) as f64 * 100.0 / total as f64
            }
        };
        let top = |m: BTreeMap<String, u64>| {
            let mut v: Vec<(String, u64)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v.into_iter().take(TOP).map(|(k, _)| k).collect::<Vec<_>>()
        };
        let mut top_orgs = Vec::new();
        for id in top(orgs) {
            if let Ok(o) = self.fg().orgs.org_any(&cx, &id).await {
                top_orgs.push(o.name);
            }
        }
        Ok(UserActivityOverview {
            login: profile
                .profile
                .as_ref()
                .map(|p| p.login.clone())
                .unwrap_or_default(),
            changesets_percent: pct(activity::CHANGESETS),
            reviews_percent: pct(activity::REVIEWS),
            issues_percent: pct(activity::ISSUES),
            change_requests_percent: pct(activity::CHANGE_REQUESTS),
            top_repos: top(repos),
            top_orgs,
            from_date: date_string(from),
            to_date: date_string(to),
        })
    }

    /// `ListUserFeedEvents`: the caller's home feed, the activity of the users they follow
    /// and the news of the repos they starred, newest first.
    pub(crate) async fn home_feed(
        &self,
        caller: &Caller,
        r: ListUserFeedEventsRequest,
    ) -> Result<ListUserFeedEventsResponse> {
        let cx = self.user_cx(caller, Action::Read)?;
        let me = caller.cx.actor_id.clone();
        let (token, limit) = page(&r.page);
        let mut before = feed_cursor(&token)?;
        let users = &self.community().users;
        let mut scopes = BTreeSet::new();
        let (followed, _) = users.following(&cx, &me, "", MAX_FEED_SOURCES).await?;
        for f in followed {
            scopes.insert(activity::user_scope(&f));
        }
        let (starred, _) = users.starred(&cx, &me, "", MAX_FEED_SOURCES).await?;
        for (repo, _) in starred {
            scopes.insert(activity::repo_scope(&repo));
        }
        let mut vis = Visible {
            svc: self,
            caller,
            cx: &cx,
            seen: BTreeMap::new(),
        };
        let mut events: Vec<(String, UserFeedEvent)> = Vec::new();
        let mut next_token = String::new();
        if scopes.is_empty() {
            return Ok(ListUserFeedEventsResponse {
                events: Vec::new(),
                page: next(next_token),
            });
        }
        // Fill one page of visible rows; each round reads past the last row considered.
        'fill: for _ in 0..20 {
            let (rows, more) = self
                .community()
                .activity
                .merged(&cx, &scopes, before.clone(), limit + 1)
                .await?;
            for p in &rows {
                before = Some((p.n1, p.id.clone()));
                let rec = &p.rec;
                if rec.actor == me
                    || !crate::events::kind_matches(&r.kinds, &rec.kind)
                    || !vis.sees(rec).await
                {
                    continue;
                }
                if events.len() == limit {
                    // A further visible row exists: the page ends at the previous one.
                    break 'fill;
                }
                events.push((feed_token(p), self.feed_event(rec).await));
            }
            if !more {
                next_token.clear();
                return Ok(ListUserFeedEventsResponse {
                    events: events.into_iter().map(|(_, e)| e).collect(),
                    page: next(next_token),
                });
            }
        }
        if let Some((t, _)) = events.last() {
            next_token = t.clone();
        }
        Ok(ListUserFeedEventsResponse {
            events: events.into_iter().map(|(_, e)| e).collect(),
            page: next(next_token),
        })
    }

    async fn feed_event(&self, rec: &ActivityRec) -> UserFeedEvent {
        let actor = self
            .fg()
            .hooks
            .directory
            .get(&rec.actor)
            .await
            .unwrap_or_else(|| principal_ref(&rec.actor));
        UserFeedEvent {
            id: rec.event_id.clone(),
            actor: Some(actor),
            kind: rec.kind.clone(),
            subject: rec.subject.clone(),
            summary: rec.summary.clone(),
            payload: rec.payload.clone(),
            created_at: Some(ts(rec.time_ms)),
        }
    }

    /// `ListUserAchievements`: achievements derived from the user's activity in repos the
    /// caller can read.
    pub(crate) async fn user_achievements(
        &self,
        caller: &Caller,
        r: ListUserAchievementsRequest,
    ) -> Result<ListUserAchievementsResponse> {
        let cx = self.user_cx(caller, Action::Read)?;
        let pid = self.uid(&cx, &r.login).await?;
        let rows = self
            .community()
            .activity
            .range(
                &cx,
                &activity::user_scope(&pid),
                0,
                self.fg().db.now_ms() + 1,
            )
            .await?;
        let mut vis = Visible {
            svc: self,
            caller,
            cx: &cx,
            seen: BTreeMap::new(),
        };
        let mut seen = Vec::new();
        for rec in rows {
            if !rec.class.is_empty() && vis.sees(&rec).await {
                seen.push(rec);
            }
        }
        let base = self.community().config.base_url.trim_end_matches('/');
        Ok(ListUserAchievementsResponse {
            achievements: activity::achievements(&seen)
                .into_iter()
                .map(|e| UserAchievement {
                    slug: e.def.slug.into(),
                    name: e.def.name.into(),
                    description: e.def.description.into(),
                    tier: TIERS[e.tier].into(),
                    count: e.count,
                    image_url: format!("{base}/achievements/{}-{}.png", e.def.slug, TIERS[e.tier]),
                    unlocked_by: e.unlocked_by.subject.clone(),
                    unlocked_at: Some(ts(e.unlocked_by.time_ms)),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_default_to_a_year_and_are_bounded() {
        let now = 1_700_000_000_000u64; // 2023-11-14
        let (f, t, _) = range("", "", "", now, 365).unwrap();
        assert_eq!((date_string(t), t - f + 1), ("2023-11-14".to_string(), 365));
        assert!(range("2023-01-02", "2023-01-01", "", now, 365).is_err());
        assert!(range("2020-01-01", "2023-01-01", "", now, 365).is_err());
        assert!(range("x", "", "", now, 365).is_err());
        let (f, t, z) = range("2023-01-01", "2023-01-01", "Asia/Kolkata", now, 365).unwrap();
        let (s, e) = bounds(&z, f, t);
        assert_eq!(e - s, DAY_MS as u64);
        assert_eq!(s % DAY_MS as u64, (DAY_MS as u64) - 330 * 60_000);
    }
}
