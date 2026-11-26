//! The Following feed: published posts of followed repos, posts by followed principals and
//! their reposts, merged in `(at, post id)` order.
//!
//! **Sources.** Each followed repo is a scan of its published posts, each followed principal
//! two: the posts they wrote and the posts they reposted. Every scan is ordered by time and
//! bounded by the cursor; a scan that returned its whole limit is *truncated* and its last
//! time is its *frontier*. Only items strictly beyond every truncated frontier are complete
//! (a truncated scan may hold more rows at its frontier's millisecond), so a round emits the
//! complete items in order, moves the cursor past the last one it looked at, and the next
//! round scans again from there; a round that made no progress doubles the scan limit.
//!
//! **Once, under its newest reason.** A post may reach the caller several ways (its repo, its
//! author, reposts). Its canonical reason is the newest: its publication, or the latest
//! repost by someone the caller follows (a repost wins a tie with the publication, and among
//! reposts in one millisecond the smallest reposter id wins). An item is emitted only when it
//! is the canonical reason of its post, so each post appears once across all pages, at the
//! position of its newest reason.
//!
//! **Visibility.** Every item's repo is checked for the caller before its post is loaded;
//! only `PUBLISHED` posts are emitted.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use bee_proto::v1::PostState;

use super::model::PostRec;
use super::store::{At, Posts};
use super::{POSTS, REPOSTS};
use crate::changes::SURFACE;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::store::{Key, MAX_SCAN, Order, Scan, Tx};

/// Most followed repos and principals a feed reads.
pub const MAX_SOURCES: usize = 1000;
/// Most reposts of one post read to find its newest reason.
const MAX_REPOSTERS: usize = 1000;
/// Rounds one page may take.
const MAX_ROUNDS: usize = 20;

/// What the caller follows.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    /// Followed repo ids.
    pub repos: BTreeSet<String>,
    /// Followed principal ids.
    pub people: BTreeSet<String>,
}

/// Why a post is in the feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Its publication (its repo or its author is followed).
    Published,
    /// A repost by this followed principal.
    Repost(String),
}

/// One feed entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// When: the publication or repost time (ms).
    pub at: i64,
    /// The post.
    pub post_id: String,
    /// Its repo.
    pub repo_id: String,
    /// Why it is here.
    pub reason: Reason,
}

impl Item {
    fn key(&self) -> (i64, &str) {
        (self.at, self.post_id.as_str())
    }
}

/// Direction of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Newest first, strictly older than the cursor.
    Older,
    /// Oldest first, strictly newer than the cursor (polling).
    Newer,
}

/// A loaded post, its version and its canonical reason.
type Judged = (PostRec, u64, (i64, Reason));

/// Decides whether the caller may read a repo (memoized by the caller).
#[async_trait]
pub trait Readable: Send {
    /// Whether the caller reads `repo_id`.
    async fn reads(&mut self, repo_id: &str) -> bool;
}

/// A page of the feed.
pub struct FeedPage {
    /// The items with their posts and versions.
    pub items: Vec<(Item, PostRec, u64)>,
    /// Where the next page starts; `None` at the end.
    pub next: Option<At>,
}

/// Parses a feed cursor, `"<at_ms>|<post_id>"`; `bad_page_token` otherwise. The id may be
/// empty (`"<at_ms>|"`), which sorts before every post id of that millisecond.
pub fn parse_cursor(s: &str) -> Result<Option<At>> {
    if s.is_empty() {
        return Ok(None);
    }
    let bad = || ForgeError::invalid("bad_page_token", format!("{s:?} is not a feed cursor"));
    let (at, id) = s.split_once('|').ok_or_else(bad)?;
    let at = at.parse::<u64>().map_err(|_| bad())?;
    if id.contains('|') || at > i64::MAX as u64 {
        return Err(bad());
    }
    Ok(Some((at as i64, id.to_string())))
}

/// The cursor text of a position.
pub fn cursor_text(at: &At) -> String {
    format!("{}|{}", at.0, at.1)
}

#[derive(Clone, Debug)]
enum Src {
    Repo(String),
    Author(String),
    Reposts(String),
}

fn beyond(dir: Dir, key: (i64, &str), cursor: &Option<At>) -> bool {
    match cursor {
        None => true,
        Some((at, id)) => {
            let c = key.cmp(&(*at, id.as_str()));
            match dir {
                Dir::Older => c == Ordering::Less,
                Dir::Newer => c == Ordering::Greater,
            }
        }
    }
}

async fn fetch(
    tx: &mut Tx,
    src: &Src,
    cursor: &Option<At>,
    dir: Dir,
    k: usize,
) -> Result<Vec<Item>> {
    let mut scan = match src {
        Src::Repo(r) => Scan::scope(r.clone(), k),
        Src::Author(a) => Scan {
            key: Some(Key::K1(a.clone())),
            limit: k,
            ..Scan::default()
        },
        Src::Reposts(p) => Scan::scope(p.clone(), k),
    };
    scan.order = Order::N1;
    scan.descending = dir == Dir::Older;
    if let Some((at, _)) = cursor {
        match dir {
            Dir::Older => scan.n1_max = Some(*at),
            Dir::Newer => scan.n1_min = Some(*at),
        }
    }
    let table = match src {
        Src::Reposts(_) => &REPOSTS,
        _ => &POSTS,
    };
    let rows = tx.scan(table, &scan).await?;
    Ok(rows
        .into_iter()
        .map(|r| match src {
            Src::Reposts(p) => Item {
                at: r.n1.unwrap_or(0),
                post_id: r.id,
                repo_id: r.k2.unwrap_or_default(),
                reason: Reason::Repost(p.clone()),
            },
            _ => Item {
                at: r.n1.unwrap_or(0),
                post_id: r.id,
                repo_id: r.scope,
                reason: Reason::Published,
            },
        })
        .collect())
}

/// The canonical reason of a post for `sources`: `(at, reason)`, or `None` when nothing the
/// caller follows leads to it.
fn canonical(rec: &PostRec, sources: &Sources, reposts: &[(String, i64)]) -> Option<(i64, Reason)> {
    let p = rec.post();
    let repo = crate::community::common::ref_repo_id(&p.repo);
    let mut reasons: Vec<(i64, Reason)> = Vec::new();
    if sources.repos.contains(&repo) || sources.people.contains(rec.author_id()) {
        reasons.push((
            p.published_at.as_ref().map(ts_ms).unwrap_or(0) as i64,
            Reason::Published,
        ));
    }
    for (who, at) in reposts {
        if sources.people.contains(who) {
            reasons.push((*at, Reason::Repost(who.clone())));
        }
    }
    reasons.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| match (&a.1, &b.1) {
            (Reason::Repost(x), Reason::Repost(y)) => x.cmp(y),
            (Reason::Repost(_), Reason::Published) => Ordering::Less,
            (Reason::Published, Reason::Repost(_)) => Ordering::Greater,
            (Reason::Published, Reason::Published) => Ordering::Equal,
        })
    });
    reasons.into_iter().next()
}

impl Posts {
    /// Reposters of a post and when they reposted it.
    async fn reposters(tx: &mut Tx, post_id: &str) -> Result<Vec<(String, i64)>> {
        let scan = Scan {
            key: Some(Key::K1(post_id.to_string())),
            limit: MAX_REPOSTERS,
            ..Scan::default()
        };
        Ok(tx
            .scan(&REPOSTS, &scan)
            .await?
            .into_iter()
            .map(|r| (r.scope, r.n1.unwrap_or(0)))
            .collect())
    }

    /// One page of the Following feed for `sources`, starting after `start` in `dir`, with at
    /// most `limit` items whose repos `judge` lets the caller read.
    pub async fn feed(
        &self,
        cx: &Cx,
        sources: &Sources,
        start: Option<At>,
        dir: Dir,
        limit: usize,
        judge: &mut dyn Readable,
    ) -> Result<FeedPage> {
        let limit = limit.max(1);
        let mut srcs: Vec<Src> = sources.repos.iter().cloned().map(Src::Repo).collect();
        for p in &sources.people {
            srcs.push(Src::Author(p.clone()));
            srcs.push(Src::Reposts(p.clone()));
        }
        let mut out: Vec<(Item, PostRec, u64)> = Vec::new();
        if srcs.is_empty() {
            return Ok(FeedPage {
                items: out,
                next: None,
            });
        }
        let mut emitted: BTreeSet<String> = BTreeSet::new();
        // Per post: its record, version and canonical reason (None: skip it).
        let mut judged: BTreeMap<String, Option<Judged>> = BTreeMap::new();
        let mut cursor = start;
        let mut k = limit + 1;
        for _ in 0..MAX_ROUNDS {
            let mut all: Vec<Item> = Vec::new();
            let mut frontier: Option<i64> = None;
            {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                for s in &srcs {
                    let rows = fetch(&mut tx, s, &cursor, dir, k).await?;
                    if rows.len() >= k
                        && let Some(last) = rows.last()
                    {
                        frontier = Some(match (frontier, dir) {
                            (None, _) => last.at,
                            (Some(f), Dir::Older) => f.max(last.at),
                            (Some(f), Dir::Newer) => f.min(last.at),
                        });
                    }
                    all.extend(rows.into_iter().filter(|i| beyond(dir, i.key(), &cursor)));
                }
            }
            all.sort_by(|a, b| match dir {
                Dir::Older => b.key().cmp(&a.key()),
                Dir::Newer => a.key().cmp(&b.key()),
            });
            let complete = |i: &Item| match (frontier, dir) {
                (None, _) => true,
                (Some(f), Dir::Older) => i.at > f,
                (Some(f), Dir::Newer) => i.at < f,
            };
            let mut last: Option<At> = None;
            for item in all.into_iter().filter(|i| complete(i)) {
                last = Some((item.at, item.post_id.clone()));
                if emitted.contains(&item.post_id) || !judge.reads(&item.repo_id).await {
                    continue;
                }
                if !judged.contains_key(&item.post_id) {
                    let mut tx = self.db.begin(SURFACE, cx).await?;
                    let entry = match tx
                        .get_msg::<PostRec>(&POSTS, &item.repo_id, &item.post_id)
                        .await?
                    {
                        Some((rec, row)) if rec.state() == PostState::Published => {
                            let reposts = Self::reposters(&mut tx, &item.post_id).await?;
                            canonical(&rec, sources, &reposts).map(|c| (rec, row.version, c))
                        }
                        _ => None,
                    };
                    judged.insert(item.post_id.clone(), entry);
                }
                let Some(Some((rec, version, (at, reason)))) = judged.get(&item.post_id) else {
                    continue;
                };
                if *at != item.at || *reason != item.reason {
                    continue;
                }
                if out.len() == limit {
                    // A further item exists: the page ends at the previous one.
                    let next = out.last().map(|(i, _, _)| (i.at, i.post_id.clone()));
                    return Ok(FeedPage { items: out, next });
                }
                emitted.insert(item.post_id.clone());
                out.push((item, rec.clone(), *version));
            }
            let Some(f) = frontier else {
                return Ok(FeedPage {
                    items: out,
                    next: None,
                });
            };
            match last {
                Some(c) => cursor = Some(c),
                None if k < MAX_SCAN => k = (k * 2).min(MAX_SCAN),
                // More than MAX_SCAN rows of one source share a millisecond: step past it.
                None => {
                    cursor = Some(match dir {
                        Dir::Older => (f, String::new()),
                        Dir::Newer => (f, "\u{10FFFF}".to_string()),
                    })
                }
            }
        }
        Ok(FeedPage {
            items: out,
            next: cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_proto::v1::{Post, Principal, RepoRef, repo_ref};

    fn rec(repo: &str, author: &str, at: u64) -> PostRec {
        PostRec {
            post: Some(Post {
                id: "p".into(),
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo.into())),
                }),
                author: Some(Principal {
                    id: author.into(),
                    ..Default::default()
                }),
                published_at: Some(crate::ids::ts(at)),
                ..Default::default()
            }),
            org_id: String::new(),
        }
    }

    #[test]
    fn cursors_round_trip_and_reject_garbage() {
        assert_eq!(parse_cursor("").unwrap(), None);
        let at = parse_cursor("1700000000000|01HX").unwrap().unwrap();
        assert_eq!(at, (1_700_000_000_000, "01HX".to_string()));
        assert_eq!(cursor_text(&at), "1700000000000|01HX");
        assert_eq!(parse_cursor("12|").unwrap(), Some((12, String::new())));
        for bad in ["x", "12", "|id", "x|id", "-1|id", "1|a|b"] {
            assert_eq!(
                parse_cursor(bad).unwrap_err().subcode,
                "bad_page_token",
                "{bad}"
            );
        }
    }

    #[test]
    fn the_newest_reason_wins() {
        let s = Sources {
            repos: ["r".to_string()].into(),
            people: ["bob".to_string(), "amy".to_string()].into(),
        };
        let r = rec("r", "x", 100);
        assert_eq!(canonical(&r, &s, &[]), Some((100, Reason::Published)));
        // A repost by someone not followed does not count.
        assert_eq!(
            canonical(&r, &s, &[("eve".into(), 300)]),
            Some((100, Reason::Published))
        );
        assert_eq!(
            canonical(&r, &s, &[("bob".into(), 200), ("amy".into(), 250)]),
            Some((250, Reason::Repost("amy".into())))
        );
        // Ties: a repost beats the publication, the smaller reposter id beats the larger.
        assert_eq!(
            canonical(&r, &s, &[("bob".into(), 100), ("amy".into(), 100)]),
            Some((100, Reason::Repost("amy".into())))
        );
        // Neither the repo nor the author is followed: only reposts lead to it.
        let other = rec("q", "x", 100);
        assert_eq!(canonical(&other, &s, &[]), None);
        assert_eq!(
            canonical(&other, &s, &[("bob".into(), 50)]),
            Some((50, Reason::Repost("bob".into())))
        );
        // A followed author leads to it in any repo.
        let by_bob = rec("q", "bob", 70);
        assert_eq!(canonical(&by_bob, &s, &[]), Some((70, Reason::Published)));
    }

    #[test]
    fn cursors_bound_both_directions() {
        let c = Some((10, "m".to_string()));
        assert!(beyond(Dir::Older, (10, "a"), &c));
        assert!(!beyond(Dir::Older, (10, "m"), &c));
        assert!(!beyond(Dir::Older, (11, "a"), &c));
        assert!(beyond(Dir::Newer, (10, "z"), &c));
        assert!(!beyond(Dir::Newer, (10, "m"), &c));
        assert!(beyond(Dir::Newer, (10, "a"), &None));
    }
}
