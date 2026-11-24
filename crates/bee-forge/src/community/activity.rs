//! Activity derived from the event logs: contribution graphs, activity overviews, the home
//! feed and achievements (PRODUCT §4.16, `docs/spec/forge-community.md` §7).
//!
//! A background pass ([`Activity::flush`]) reads every org log and the system log from its
//! own cursors and indexes the events people care about twice: by actor (`u:<principal>`)
//! and, for repo news, by repo (`r:<repo id>`). Everything the user-facing RPCs return is a
//! pure function of those rows; the service layer drops rows of repos the caller cannot
//! read.
//!
//! Contribution classes (the ref log and the change, review and issue events):
//!
//! | class | event |
//! |---|---|
//! | `changesets` | `ref.updated` on a branch (one per landed update) |
//! | `change_requests` | `cr.opened` |
//! | `reviews` | `review.submitted` |
//! | `issues` | `issue.opened` |
//!
//! `cr.landed` also credits the change request's author with a `landed` row, used only by
//! achievements.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::event::Body;
use bee_proto::v1::{Event, SubjectKind, SubjectRef};
use prost::Message;
use serde_json::Value;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventLog;
use crate::store::{Cursor, Db, Index, Row, Scan, Surface, TableDef};

use super::common::subject;
use super::users::USERS;

/// Indexed activity: scope `u:<principal>` or `r:<repo id>`, id the event id, `n1` the event
/// time (ms).
pub static ACTIVITY: TableDef = TableDef {
    name: "forge_activity",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};
/// Per event-log cursors of the activity pass: id the log, `n1` the next sequence to read.
pub static CURSORS: TableDef = TableDef {
    name: "forge_activity_cursors",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Landed branch updates.
pub const CHANGESETS: &str = "changesets";
/// Opened change requests.
pub const CHANGE_REQUESTS: &str = "change_requests";
/// Submitted reviews.
pub const REVIEWS: &str = "reviews";
/// Opened issues.
pub const ISSUES: &str = "issues";
/// Change requests of the user that landed (achievements only).
pub const LANDED: &str = "landed";
/// The classes a contribution graph counts.
pub const CONTRIBUTION_CLASSES: [&str; 4] = [CHANGESETS, CHANGE_REQUESTS, REVIEWS, ISSUES];

/// Events read per log and batch.
const BATCH: usize = 500;
/// Most rows one range read returns.
pub const MAX_RANGE_ROWS: usize = 100_000;

/// One indexed event.
#[derive(Clone, PartialEq, Message)]
pub struct ActivityRec {
    /// `<log>#<seq>`: the same for every row of one event.
    #[prost(string, tag = "1")]
    pub event_id: String,
    /// Event kind.
    #[prost(string, tag = "2")]
    pub kind: String,
    /// Who acted.
    #[prost(string, tag = "3")]
    pub actor: String,
    /// Org of the subject (may be empty).
    #[prost(string, tag = "4")]
    pub org_id: String,
    /// Repo of the subject (may be empty).
    #[prost(string, tag = "5")]
    pub repo_id: String,
    /// The subject.
    #[prost(message, optional, tag = "6")]
    pub subject: Option<SubjectRef>,
    /// Kind-specific JSON detail.
    #[prost(string, tag = "7")]
    pub payload: String,
    /// Event time (ms).
    #[prost(uint64, tag = "8")]
    pub time_ms: u64,
    /// Contribution class, [`LANDED`], or empty.
    #[prost(string, tag = "9")]
    pub class: String,
    /// Whether the row belongs in feeds.
    #[prost(bool, tag = "10")]
    pub feed: bool,
    /// One line of text.
    #[prost(string, tag = "11")]
    pub summary: String,
}

/// A row and its position (for paging).
#[derive(Clone, Debug, PartialEq)]
pub struct Positioned {
    /// The record.
    pub rec: ActivityRec,
    /// Time (the row's `n1`).
    pub n1: i64,
    /// Row id (the event id, or the event id plus a suffix).
    pub id: String,
}

/// The actor scope of a principal.
pub fn user_scope(principal: &str) -> String {
    format!("u:{principal}")
}

/// The repo scope of a repo.
pub fn repo_scope(repo_id: &str) -> String {
    format!("r:{repo_id}")
}

fn json(e: &Event) -> Value {
    serde_json::from_str(&e.payload_json).unwrap_or(Value::Null)
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// The rows one event yields, as `(scope, row id, record)`; pure.
pub fn derive(e: &Event, event_id: &str, time_ms: u64) -> Vec<(String, String, ActivityRec)> {
    if e.principal.is_empty() {
        return Vec::new();
    }
    let base = ActivityRec {
        event_id: event_id.to_string(),
        kind: e.kind.clone(),
        actor: e.principal.clone(),
        org_id: e.org_id.clone(),
        repo_id: e.repo_id.clone(),
        subject: e.subject.clone(),
        payload: e.payload_json.clone(),
        time_ms,
        class: String::new(),
        feed: true,
        summary: String::new(),
    };
    let me = user_scope(&e.principal);
    let mut out = Vec::new();
    let mut put = |scope: String, suffix: &str, rec: ActivityRec| {
        out.push((scope, format!("{event_id}{suffix}"), rec));
    };
    match (e.kind.as_str(), &e.body) {
        ("ref.updated", Some(Body::RefUpdated(r))) => {
            let deleted = r.target.as_ref().is_none_or(|t| t.raw.is_empty());
            if r.name.starts_with("tags/") || deleted {
                return Vec::new();
            }
            let rec = ActivityRec {
                class: CHANGESETS.into(),
                summary: format!("updated {}", r.name),
                subject: e
                    .subject
                    .clone()
                    .or_else(|| Some(subject(SubjectKind::Repo, &e.repo_id, &e.repo_id))),
                ..base
            };
            put(me, "", rec);
        }
        ("cr.opened", Some(Body::Change(cr))) => {
            let rec = ActivityRec {
                class: CHANGE_REQUESTS.into(),
                summary: format!("opened change request #{}: {}", cr.number, cr.title),
                ..base
            };
            put(me, "", rec);
        }
        ("cr.landed", Some(Body::Change(cr))) => {
            let rec = ActivityRec {
                summary: format!("landed change request #{}: {}", cr.number, cr.title),
                ..base
            };
            put(me, "", rec.clone());
            if !e.repo_id.is_empty() {
                put(repo_scope(&e.repo_id), "", rec.clone());
            }
            if let Some(author) = cr.author.as_ref().filter(|a| !a.id.is_empty()) {
                let credit = ActivityRec {
                    class: LANDED.into(),
                    feed: false,
                    ..rec
                };
                put(user_scope(&author.id), "#landed", credit);
            }
        }
        ("review.submitted", Some(Body::Review(rv))) => {
            let rec = ActivityRec {
                class: REVIEWS.into(),
                summary: format!("reviewed change request {}", rv.change_id),
                ..base
            };
            put(me, "", rec);
        }
        ("issue.opened", _) => {
            let v = json(e);
            let rec = ActivityRec {
                class: ISSUES.into(),
                summary: format!(
                    "opened issue #{}: {}",
                    text(&v, "number"),
                    text(&v, "title")
                ),
                ..base
            };
            put(me, "", rec);
        }
        ("release.published", _) => {
            let v = json(e);
            let mut name = text(&v, "name");
            if name.is_empty() {
                name = text(&v, "tagName");
            }
            let rec = ActivityRec {
                summary: format!("published release {name}"),
                ..base
            };
            put(me, "", rec.clone());
            if !e.repo_id.is_empty() {
                put(repo_scope(&e.repo_id), "", rec);
            }
        }
        ("discussion.created", _) => {
            let v = json(e);
            let rec = ActivityRec {
                summary: format!("started a discussion: {}", text(&v, "title")),
                ..base
            };
            put(me, "", rec.clone());
            if !e.repo_id.is_empty() {
                put(repo_scope(&e.repo_id), "", rec);
            }
        }
        ("star.added", _) => {
            let rec = ActivityRec {
                summary: format!("starred {}", text(&json(e), "repo")),
                ..base
            };
            put(me, "", rec);
        }
        ("follow.added", _) => {
            let rec = ActivityRec {
                summary: format!("followed {}", text(&json(e), "followee")),
                ..base
            };
            put(me, "", rec);
        }
        ("repo.created", _) => {
            let rec = ActivityRec {
                summary: "created a repository".into(),
                ..base
            };
            put(me, "", rec);
        }
        _ => {}
    }
    out
}

/// Intensity bucket (0..=4) of `count` on a graph whose busiest day has `max`.
pub fn level(count: u32, max: u32) -> u32 {
    if count == 0 || max == 0 {
        return 0;
    }
    (count * 4).div_ceil(max).clamp(1, 4)
}

/// An achievement definition: counts of one class, with tier thresholds.
pub struct AchievementDef {
    /// Stable slug.
    pub slug: &'static str,
    /// Display name.
    pub name: &'static str,
    /// What earns it.
    pub description: &'static str,
    /// The class counted.
    pub class: &'static str,
    /// Thresholds of the tiers in [`TIERS`] order (the first unlocks the achievement).
    pub thresholds: [u32; 4],
}

/// Tier names, lowest first.
pub const TIERS: [&str; 4] = ["base", "bronze", "silver", "gold"];

/// Every achievement.
pub const ACHIEVEMENTS: &[AchievementDef] = &[
    AchievementDef {
        slug: "first-landing",
        name: "First landing",
        description: "Landed changesets on a branch",
        class: CHANGESETS,
        thresholds: [1, 10, 100, 1000],
    },
    AchievementDef {
        slug: "change-author",
        name: "Change author",
        description: "Opened change requests",
        class: CHANGE_REQUESTS,
        thresholds: [1, 10, 50, 250],
    },
    AchievementDef {
        slug: "shipped",
        name: "Shipped",
        description: "Authored change requests that landed",
        class: LANDED,
        thresholds: [1, 16, 128, 1024],
    },
    AchievementDef {
        slug: "second-pair-of-eyes",
        name: "Second pair of eyes",
        description: "Reviewed change requests",
        class: REVIEWS,
        thresholds: [1, 10, 50, 250],
    },
    AchievementDef {
        slug: "bug-reporter",
        name: "Bug reporter",
        description: "Opened issues",
        class: ISSUES,
        thresholds: [1, 10, 50, 250],
    },
];

/// An achievement reached: the definition, the count, the tier and the record that unlocked
/// it.
pub struct Earned {
    /// The definition.
    pub def: &'static AchievementDef,
    /// Rows of the class counted.
    pub count: u32,
    /// Index into [`TIERS`].
    pub tier: usize,
    /// The row that crossed the first threshold.
    pub unlocked_by: ActivityRec,
}

/// Achievements earned by `rows` (any order); pure.
pub fn achievements(rows: &[ActivityRec]) -> Vec<Earned> {
    let mut by_class: BTreeMap<&str, Vec<&ActivityRec>> = BTreeMap::new();
    for r in rows {
        by_class.entry(r.class.as_str()).or_default().push(r);
    }
    let mut out = Vec::new();
    for def in ACHIEVEMENTS {
        let Some(list) = by_class.get_mut(def.class) else {
            continue;
        };
        list.sort_by(|a, b| (a.time_ms, &a.event_id).cmp(&(b.time_ms, &b.event_id)));
        let count = list.len() as u32;
        if count < def.thresholds[0] {
            continue;
        }
        let tier = def
            .thresholds
            .iter()
            .rposition(|t| count >= *t)
            .unwrap_or(0);
        out.push(Earned {
            def,
            count,
            tier,
            unlocked_by: list[def.thresholds[0] as usize - 1].clone(),
        });
    }
    out
}

/// The activity index.
pub struct Activity {
    db: Db,
}

impl Activity {
    /// The index over `db`.
    pub fn new(db: Db) -> Activity {
        Activity { db }
    }

    /// Indexes events appended to the org logs and the system log since the last pass;
    /// returns how many events were read.
    pub async fn flush(&self, cx: &Cx, events: &EventLog) -> Result<usize> {
        let logs: Vec<String> = {
            let mut tx = self.db.begin(Surface::EVENTS, cx).await?;
            let rows = tx
                .scan(
                    &crate::events::COUNTERS,
                    &Scan {
                        limit: crate::store::MAX_SCAN,
                        ..Scan::default()
                    },
                )
                .await?;
            rows.into_iter()
                .filter(|r| r.id == "seq" && (r.scope.starts_with("org:") || r.scope == "system"))
                .map(|r| r.scope)
                .collect()
        };
        let mut read = 0;
        for log in logs {
            loop {
                let cursor = {
                    let mut tx = self.db.begin(USERS, cx).await?;
                    tx.get(&CURSORS, "", &log)
                        .await?
                        .and_then(|r| r.n1)
                        .unwrap_or(1)
                        .max(1) as u64
                };
                let (batch, next) = match events.read(cx, &log, cursor, &[], BATCH).await {
                    Ok(x) => x,
                    Err(e) if e.subcode == "seq_not_retained" => {
                        events.read(cx, &log, 0, &[], BATCH).await?
                    }
                    Err(e) => return Err(e),
                };
                if next <= cursor {
                    break;
                }
                let mut tx = self.db.begin(USERS, cx).await?;
                for e in &batch {
                    let time = e.time.as_ref().map(crate::ids::ts_ms).unwrap_or(0);
                    let event_id = format!("{log}#{:020}", e.seq);
                    for (scope, id, rec) in derive(e, &event_id, time) {
                        tx.put(&ACTIVITY, &Row::new(scope, id).n1(time as i64).msg(&rec))
                            .await?;
                    }
                }
                tx.put(&CURSORS, &Row::new("", log.clone()).n1(next as i64))
                    .await?;
                tx.commit().await?;
                read += batch.len();
                if batch.len() < BATCH {
                    break;
                }
            }
        }
        Ok(read)
    }

    /// Rows of `scope` with `from_ms <= time < to_ms`, oldest first (at most
    /// [`MAX_RANGE_ROWS`]).
    pub async fn range(
        &self,
        cx: &Cx,
        scope: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<Vec<ActivityRec>> {
        let mut out = Vec::new();
        if to_ms <= from_ms {
            return Ok(out);
        }
        let mut tx = self.db.begin(USERS, cx).await?;
        let mut after: Option<Cursor> = None;
        loop {
            let scan = Scan {
                n1_min: Some(from_ms as i64),
                n1_max: Some(to_ms as i64 - 1),
                ..Scan::scope(scope, crate::store::MAX_SCAN)
                    .by_n1()
                    .after(after.clone())
            };
            let rows = tx.scan(&ACTIVITY, &scan).await?;
            let full = rows.len() == crate::store::MAX_SCAN;
            for row in &rows {
                out.push(row.decode::<ActivityRec>()?);
            }
            after = rows.last().map(Cursor::after);
            if !full || out.len() >= MAX_RANGE_ROWS {
                break;
            }
        }
        out.truncate(MAX_RANGE_ROWS);
        Ok(out)
    }

    /// The newest feed rows across `scopes` strictly before `before` (`(time, row id)`, or
    /// the newest when `None`), newest first, one row per event, at most `want`; and whether
    /// older rows remain.
    pub async fn merged(
        &self,
        cx: &Cx,
        scopes: &BTreeSet<String>,
        before: Option<(i64, String)>,
        want: usize,
    ) -> Result<(Vec<Positioned>, bool)> {
        let want = want.clamp(1, 1000);
        let mut tx = self.db.begin(USERS, cx).await?;
        let mut all: Vec<Positioned> = Vec::new();
        let mut more = false;
        for scope in scopes {
            let after = before.as_ref().map(|(n1, id)| Cursor {
                n1: *n1,
                scope: scope.clone(),
                id: id.clone(),
            });
            let mut cursor = after;
            let mut got = 0;
            // Rows that are not feed rows do not count towards `want`.
            for _ in 0..20 {
                let rows = tx
                    .scan(
                        &ACTIVITY,
                        &Scan::scope(scope.clone(), want)
                            .by_n1()
                            .desc()
                            .after(cursor.clone()),
                    )
                    .await?;
                let full = rows.len() == want;
                cursor = rows.last().map(Cursor::after);
                for row in rows {
                    let rec = row.decode::<ActivityRec>()?;
                    if !rec.feed {
                        continue;
                    }
                    all.push(Positioned {
                        rec,
                        n1: row.n1.unwrap_or(0),
                        id: row.id,
                    });
                    got += 1;
                }
                if !full {
                    break;
                }
                if got >= want {
                    more = true;
                    break;
                }
            }
        }
        all.sort_by(|a, b| (b.n1, &b.id).cmp(&(a.n1, &a.id)));
        let mut seen = BTreeSet::new();
        all.retain(|p| seen.insert(p.rec.event_id.clone()));
        if all.len() > want {
            all.truncate(want);
            more = true;
        }
        Ok((all, more))
    }
}

/// Encodes a feed position as a page token.
pub fn feed_token(p: &Positioned) -> String {
    format!("{}|{}", p.n1, p.id)
}

/// Decodes a feed page token; empty is the start.
pub fn feed_cursor(token: &str) -> Result<Option<(i64, String)>> {
    if token.is_empty() {
        return Ok(None);
    }
    let bad = || ForgeError::invalid("bad_page_token", "the page token is not valid");
    let (n1, id) = token.split_once('|').ok_or_else(bad)?;
    Ok(Some((n1.parse().map_err(|_| bad())?, id.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_proto::v1::{ChangeRequest, ObjectId, Principal, Ref};

    fn ev(kind: &str, who: &str, body: Option<Body>) -> Event {
        Event {
            kind: kind.into(),
            principal: who.into(),
            repo_id: "R".into(),
            org_id: "O".into(),
            body,
            ..Default::default()
        }
    }

    #[test]
    fn events_map_to_classes_and_scopes() {
        let r = Ref {
            name: "main".into(),
            target: Some(ObjectId { raw: vec![1] }),
            ..Default::default()
        };
        let rows = derive(
            &ev("ref.updated", "ada", Some(Body::RefUpdated(r))),
            "e1",
            5,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].0.as_str(), rows[0].2.class.as_str()),
            ("u:ada", CHANGESETS)
        );
        let tag = Ref {
            name: "tags/v1".into(),
            target: Some(ObjectId { raw: vec![1] }),
            ..Default::default()
        };
        assert!(
            derive(
                &ev("ref.updated", "ada", Some(Body::RefUpdated(tag))),
                "e2",
                5
            )
            .is_empty()
        );
        let cr = ChangeRequest {
            number: 3,
            title: "t".into(),
            author: Some(Principal {
                id: "bob".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let landed = derive(&ev("cr.landed", "ada", Some(Body::Change(cr))), "e3", 9);
        let scopes: Vec<(&str, &str, bool)> = landed
            .iter()
            .map(|(s, _, r)| (s.as_str(), r.class.as_str(), r.feed))
            .collect();
        assert_eq!(
            scopes,
            vec![
                ("u:ada", "", true),
                ("r:R", "", true),
                ("u:bob", LANDED, false)
            ]
        );
        assert!(derive(&ev("cr.landed", "", None), "e4", 9).is_empty());
    }

    #[test]
    fn levels_and_achievement_tiers() {
        assert_eq!([0, 1, 2, 3, 4, 8].map(|c| level(c, 8)), [0, 1, 1, 2, 2, 4]);
        let rows: Vec<ActivityRec> = (0..12)
            .map(|i| ActivityRec {
                event_id: format!("e{i:02}"),
                class: REVIEWS.into(),
                time_ms: 100 - i,
                ..Default::default()
            })
            .collect();
        let got = achievements(&rows);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].def.slug, got[0].count, TIERS[got[0].tier]),
            ("second-pair-of-eyes", 12, "bronze")
        );
        assert_eq!(
            got[0].unlocked_by.event_id, "e11",
            "the oldest row unlocks it"
        );
    }

    #[test]
    fn feed_tokens_round_trip() {
        let p = Positioned {
            rec: ActivityRec::default(),
            n1: 42,
            id: "org:x#1".into(),
        };
        assert_eq!(
            feed_cursor(&feed_token(&p)).unwrap(),
            Some((42, "org:x#1".into()))
        );
        assert!(feed_cursor("nope").is_err());
    }
}
