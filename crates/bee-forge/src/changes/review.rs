//! Reviews, review requests and auto-assignment, review threads with carry-forward,
//! suggestions, "viewed" files and revision diffs (SPEC §11.2, §11.4).
//!
//! **Anchors.** A thread anchors to `(path, side, start..=end)` on a revision: side `new`
//! reads the revision's head, side `old` its base; a file-level thread has no lines. When a
//! revision is added, each live thread is compared between the revision it was last valid
//! on and the new one ([`super::text::carry_range`]): unchanged lines move with the edits
//! above them and the thread advances to the new revision; otherwise it is marked outdated
//! and keeps its old revision.
//!
//! **Stale approvals.** Approvals of older revisions are `stale`. With the land rule's
//! `dismiss_stale_approvals` they are also dismissed when the revision is pushed (policy
//! ignores them either way, SPEC §9.4).
//!
//! **Auto-assignment.** Opening a CR (or marking a draft ready) requests reviews from the
//! owners of its paths (`[[owners]]` or `CODEOWNERS`): principals directly, groups as team
//! requests. A team request is expanded by the team's review assignment: round robin over
//! a per-team cursor, or load balance by outstanding requests in the repo.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::{
    ChangeRequest, DiffItem, Event, Principal, PrincipalKind, Repo, Review, ReviewComment,
    ReviewRequest, ReviewThread, SubjectKind, Suggestion, TeamReviewAlgorithm, Verdict, ViewedFile,
    event,
};

use super::model::{CommentRecord, CrRecord, ViewedRecord, reaction_groups};
use super::store::{
    Changes, current_rev, head_of, merge_requests, repo_ref, require_open, same_request,
};
use super::text::{self, SuggestionEdit};
use super::vcs::NewCommit;
use super::{COMMENTS, COUNTERS, REVIEWS, SURFACE, THREADS, VIEWED};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Cursor, Key, Row, Scan, Tx};

/// Most comments in one review submission or one suggestion batch.
pub const MAX_REVIEW_COMMENTS: usize = 100;
/// Most threads on one CR.
pub const MAX_THREADS: usize = 5000;

/// What a revision diff reports.
#[derive(Clone, Debug, Default)]
pub struct DiffFilter {
    /// Only these paths and the paths under them; empty = every path.
    pub paths: Vec<String>,
    /// Leave whitespace-only line changes (and files with nothing else) out.
    pub ignore_whitespace: bool,
}

/// A validated anchor.
#[derive(Clone, Debug, Default)]
struct Anchor {
    path: String,
    side: String,
    start: u32,
    end: u32,
    file_level: bool,
    revision: u32,
    outdated: bool,
}

fn verdict_of(r: &Review) -> Verdict {
    Verdict::try_from(r.verdict).unwrap_or(Verdict::Unspecified)
}

fn review_json(r: &Review) -> String {
    serde_json::json!({
        "id": r.id,
        "change_id": r.change_id,
        "revision": r.revision,
        "verdict": verdict_of(r).as_str_name(),
        "dismissed": r.dismissed,
        "stale": r.stale,
    })
    .to_string()
}

impl Changes {
    // ----- auto-assignment ------------------------------------------------------------------

    /// Review requests for the owners of the CR's paths that `existing` does not cover.
    pub(crate) async fn owner_requests(
        &self,
        repo: &Repo,
        rec: &CrRecord,
        existing: &[ReviewRequest],
    ) -> Result<Vec<ReviewRequest>> {
        let Some(policy) = self.policy_of(repo).await? else {
            return Ok(Vec::new());
        };
        let author = rec
            .cr()
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        let mut out: Vec<ReviewRequest> = Vec::new();
        let now = self.db.now_ms();
        for p in &rec.paths {
            let Some((_, owners)) = policy.owners_of(&p.path) else {
                continue;
            };
            for o in owners {
                let req = match o {
                    bee_policy::OwnerRef::Principal(id) => {
                        if id.as_str() == author {
                            continue;
                        }
                        ReviewRequest {
                            reviewer: Some(self.principal(id.as_str()).await),
                            ..Default::default()
                        }
                    }
                    bee_policy::OwnerRef::Group(g) => ReviewRequest {
                        team_id: g.clone(),
                        ..Default::default()
                    },
                    bee_policy::OwnerRef::Login(l) => match self.directory.by_login(&l).await {
                        Some(pr) if pr.id != author => ReviewRequest {
                            reviewer: Some(pr),
                            ..Default::default()
                        },
                        _ => continue,
                    },
                    bee_policy::OwnerRef::Email(_) => continue,
                };
                let req = ReviewRequest {
                    requested_at: Some(ts(now)),
                    auto_assigned: true,
                    ..req
                };
                if !existing.iter().any(|e| same_request(e, &req))
                    && !out.iter().any(|e| same_request(e, &req))
                {
                    out.push(req);
                }
            }
        }
        Ok(out)
    }

    /// Expands team requests by each team's review assignment.
    pub(crate) async fn expand_teams(
        &self,
        cx: &Cx,
        repo: &Repo,
        rec: &CrRecord,
        requests: Vec<ReviewRequest>,
    ) -> Result<Vec<ReviewRequest>> {
        let author = rec
            .cr()
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_else(|| cx.actor_id.clone());
        let mut out: Vec<ReviewRequest> = Vec::new();
        let mut taken: BTreeSet<String> = rec
            .cr()
            .review_requests
            .iter()
            .filter_map(|r| r.reviewer.as_ref().map(|p| p.id.clone()))
            .collect();
        taken.extend(
            requests
                .iter()
                .filter_map(|r| r.reviewer.as_ref().map(|p| p.id.clone())),
        );
        let mut load: Option<BTreeMap<String, usize>> = None;
        for r in requests {
            if r.team_id.is_empty() {
                out.push(r);
                continue;
            }
            let Some(a) = self.teams.assignment(&repo.org_id, &r.team_id).await? else {
                out.push(r);
                continue;
            };
            if !a.enabled {
                out.push(r);
                continue;
            }
            let members = self.teams.members(&repo.org_id, &r.team_id).await?;
            let eligible: Vec<Principal> = members
                .into_iter()
                .filter(|m| m.id != author && !a.excluded_member_ids.contains(&m.id))
                .filter(|m| !(a.skip_agents && m.kind == PrincipalKind::Agent as i32))
                .collect();
            let want = a.member_count.max(1) as usize;
            let picked: Vec<Principal> = if TeamReviewAlgorithm::try_from(a.algorithm)
                .unwrap_or(TeamReviewAlgorithm::RoundRobin)
                == TeamReviewAlgorithm::LoadBalance
            {
                if load.is_none() {
                    let mut m = BTreeMap::new();
                    for (other, _) in self.records(cx, &repo.id).await? {
                        if other.cr().state != bee_proto::v1::ChangeState::Open as i32 {
                            continue;
                        }
                        for rr in &other.cr().review_requests {
                            if let Some(p) = &rr.reviewer {
                                *m.entry(p.id.clone()).or_insert(0usize) += 1;
                            }
                        }
                    }
                    load = Some(m);
                }
                let l = load.as_ref().cloned().unwrap_or_default();
                let mut c: Vec<&Principal> =
                    eligible.iter().filter(|m| !taken.contains(&m.id)).collect();
                c.sort_by_key(|m| (l.get(&m.id).copied().unwrap_or(0), m.id.clone()));
                c.into_iter().take(want).cloned().collect()
            } else if eligible.is_empty() {
                Vec::new()
            } else {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                let scope = format!("rr:{}", repo.org_id);
                let end = tx
                    .add_n1(&COUNTERS, &scope, &r.team_id, want as i64)
                    .await?;
                tx.commit().await?;
                let start = (end - want as i64).rem_euclid(eligible.len() as i64) as usize;
                (0..eligible.len())
                    .map(|i| &eligible[(start + i) % eligible.len()])
                    .filter(|m| !taken.contains(&m.id))
                    .take(want)
                    .cloned()
                    .collect()
            };
            for p in picked {
                taken.insert(p.id.clone());
                if let Some(l) = load.as_mut() {
                    *l.entry(p.id.clone()).or_insert(0) += 1;
                }
                out.push(ReviewRequest {
                    reviewer: Some(p),
                    team_id: String::new(),
                    requested_by: r.requested_by.clone(),
                    requested_at: r.requested_at,
                    auto_assigned: true,
                });
            }
            if a.notify_team {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Requests reviews from principals and teams.
    pub async fn request_reviewers(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        reviewer_ids: &[String],
        team_ids: &[String],
    ) -> Result<ChangeRequest> {
        if reviewer_ids.is_empty() && team_ids.is_empty() {
            return Err(ForgeError::missing("reviewer_ids"));
        }
        let (rec, _) = self.record(cx, repo, id).await?;
        require_open(rec.cr())?;
        let author_id = rec
            .cr()
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        if reviewer_ids.contains(&author_id) {
            return Err(ForgeError::invalid(
                "self_review_request",
                "the author cannot be requested as a reviewer",
            ));
        }
        let by = self.actor(cx).await;
        let now = self.db.now_ms();
        let mut reqs = Vec::new();
        for r in reviewer_ids {
            reqs.push(ReviewRequest {
                reviewer: Some(self.principal(r).await),
                requested_by: Some(by.clone()),
                requested_at: Some(ts(now)),
                ..Default::default()
            });
        }
        for t in team_ids {
            reqs.push(ReviewRequest {
                team_id: t.clone(),
                requested_by: Some(by.clone()),
                requested_at: Some(ts(now)),
                ..Default::default()
            });
        }
        let reqs = self.expand_teams(cx, repo, &rec, reqs).await?;
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            require_open(rec.cr())?;
            let cr = rec.cr_mut();
            if !merge_requests(&mut cr.review_requests, reqs) {
                return Ok(None);
            }
            if cr.review_requests.len() > super::store::MAX_ITEMS {
                return Err(ForgeError::too_large(
                    "review_requests",
                    super::store::MAX_ITEMS as u64,
                    cr.review_requests.len() as u64,
                ));
            }
            ev.push(("cr.review_requested", String::new()));
            Ok(Some("cr.request_reviewers".into()))
        })
        .await
    }

    /// Removes pending review requests.
    pub async fn remove_review_requests(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        reviewer_ids: &[String],
        team_ids: &[String],
    ) -> Result<ChangeRequest> {
        let (r, t) = (reviewer_ids.to_vec(), team_ids.to_vec());
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            let cr = rec.cr_mut();
            let before = cr.review_requests.len();
            cr.review_requests.retain(|q| match &q.reviewer {
                Some(p) => !r.contains(&p.id),
                None => !t.contains(&q.team_id),
            });
            if cr.review_requests.len() == before {
                return Ok(None);
            }
            ev.push(("cr.review_request_removed", String::new()));
            Ok(Some("cr.remove_review_requests".into()))
        })
        .await
    }

    // ----- anchors and threads ---------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn anchor(
        &self,
        repo: &Repo,
        cr: &ChangeRequest,
        path: &[u8],
        side: &str,
        start: u32,
        end: u32,
        file_level: bool,
        revision: u32,
    ) -> Result<Anchor> {
        let path = std::str::from_utf8(path)
            .map_err(|_| ForgeError::invalid("invalid_path", "paths must be UTF-8"))?
            .trim_matches('/')
            .to_string();
        if path.is_empty() {
            return Err(ForgeError::missing("path"));
        }
        let side = match side {
            "" | "new" | "right" | "RIGHT" => "new",
            "old" | "left" | "LEFT" => "old",
            other => {
                return Err(ForgeError::invalid(
                    "invalid_side",
                    format!("side must be \"old\" or \"new\", not {other:?}"),
                ));
            }
        }
        .to_string();
        let current = current_rev(cr);
        let r = if revision == 0 { current } else { revision };
        let rev = cr
            .revisions
            .get(r.saturating_sub(1) as usize)
            .filter(|_| r >= 1)
            .ok_or_else(|| ForgeError::invalid("invalid_revision", format!("no revision {r}")))?;
        let at = if side == "new" { &rev.head } else { &rev.base };
        let at = at.clone().unwrap_or_default();
        let content = self.vcs.read_file(repo, &at, &path).await?.ok_or_else(|| {
            ForgeError::invalid(
                "invalid_anchor",
                format!("{path} does not exist on the {side} side of revision {r}"),
            )
        })?;
        let (mut start, mut end) = if file_level || start == 0 {
            (0, 0)
        } else {
            (start, end.max(start))
        };
        if start > 0 {
            let lines = bee_merge::split_lines(&content).len() as u32;
            if end > lines {
                return Err(ForgeError::invalid(
                    "invalid_anchor",
                    format!("{path} has {lines} lines on that side; line {end} is outside it"),
                ));
            }
        }
        let mut anchor = Anchor {
            path: path.clone(),
            side: side.clone(),
            start,
            end,
            file_level: start == 0,
            revision: r,
            outdated: false,
        };
        if r < current {
            let cur = cr.revisions.last().cloned().unwrap_or_default();
            let now_at = if side == "new" { cur.head } else { cur.base }.unwrap_or_default();
            let new_content = self.vcs.read_file(repo, &now_at, &path).await?;
            match new_content.and_then(|n| text::carry_range(&content, &n, start, end)) {
                Some((s, e)) => {
                    start = s;
                    end = e;
                    anchor.start = start;
                    anchor.end = end;
                    anchor.revision = current;
                }
                None => anchor.outdated = true,
            }
        }
        Ok(anchor)
    }

    /// Carries the CR's live threads to revision `n` (head `new_head`, base `new_base`);
    /// returns the threads that changed.
    pub(crate) async fn carry_threads(
        &self,
        cx: &Cx,
        repo: &Repo,
        rec: &CrRecord,
        new_head: &bee_proto::v1::ObjectId,
        new_base: &bee_proto::v1::ObjectId,
        n: u32,
    ) -> Result<Vec<(ReviewThread, Row)>> {
        let cr = rec.cr();
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<ReviewThread>(
                &THREADS,
                &Scan::scope(repo.id.clone(), MAX_THREADS).key(Key::K1(cr.id.clone())),
            )
            .await?
        };
        let mut cache: BTreeMap<(Vec<u8>, String), Option<Vec<u8>>> = BTreeMap::new();
        let mut out = Vec::new();
        for (mut t, row) in rows {
            if t.outdated || t.revision >= n || t.revision == 0 {
                continue;
            }
            let Some(rev) = cr.revisions.get(t.revision as usize - 1) else {
                continue;
            };
            let path = String::from_utf8_lossy(&t.path).into_owned();
            let (old_at, new_at) = if t.side == "old" {
                (rev.base.clone().unwrap_or_default(), new_base.clone())
            } else {
                (rev.head.clone().unwrap_or_default(), new_head.clone())
            };
            let mut read = async |at: &bee_proto::v1::ObjectId| -> Result<Option<Vec<u8>>> {
                let key = (at.raw.clone(), path.clone());
                if let Some(v) = cache.get(&key) {
                    return Ok(v.clone());
                }
                let v = self.vcs.read_file(repo, at, &path).await?;
                cache.insert(key, v.clone());
                Ok(v)
            };
            let old = read(&old_at).await?;
            let new = read(&new_at).await?;
            let carried = match (old, new) {
                (Some(o), Some(nw)) => text::carry_range(&o, &nw, t.start_line, t.end_line),
                _ => None,
            };
            match carried {
                Some((s, e)) => {
                    t.start_line = s;
                    t.end_line = e;
                    t.revision = n;
                }
                None => t.outdated = true,
            }
            out.push((t, row));
        }
        Ok(out)
    }

    /// Writes carried threads; newly outdated ones emit `review_thread.outdated`.
    pub(crate) async fn store_threads(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        rec: &CrRecord,
        threads: Vec<(ReviewThread, Row)>,
    ) -> Result<()> {
        for (t, row) in threads {
            tx.update(&THREADS, &row.clone().msg(&t)).await?;
            if t.outdated {
                Self::emit_thread(tx, repo, rec, "review_thread.outdated", &t).await?;
            }
        }
        Ok(())
    }

    async fn emit_thread(
        tx: &mut Tx,
        repo: &Repo,
        rec: &CrRecord,
        kind: &str,
        t: &ReviewThread,
    ) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: repo.id.clone(),
            org_id: rec.org_id.clone(),
            subject: Some(bee_proto::v1::SubjectRef {
                kind: SubjectKind::ChangeRequest as i32,
                repo: repo_ref(repo),
                id: rec.cr().id.clone(),
            }),
            payload_json: serde_json::to_string(t).unwrap_or_default(),
            ..Default::default()
        })
        .await
    }

    /// Marks approvals of revisions before `n` stale, and with `dismiss` also dismisses
    /// them (`review.dismissed`).
    pub(crate) async fn mark_stale_reviews(
        &self,
        tx: &mut Tx,
        rec: &CrRecord,
        n: u32,
        dismiss: bool,
    ) -> Result<()> {
        let cr = rec.cr();
        let rows = tx
            .scan_msgs::<Review>(&REVIEWS, &Scan::scope(cr.id.clone(), 10_000))
            .await?;
        for (mut r, row) in rows {
            if r.dismissed || r.stale || r.revision >= n || verdict_of(&r) != Verdict::Approve {
                continue;
            }
            r.stale = true;
            if dismiss {
                r.dismissed = true;
                r.dismiss_reason = format!("stale: revision {n} was pushed");
            }
            tx.update(&REVIEWS, &row.clone().msg(&r)).await?;
            if dismiss {
                Self::emit_review(tx, rec, "review.dismissed", &r).await?;
                tx.audit(
                    AuditDraft::new("review.dismiss_stale", &rec.org_id)
                        .repo_id(&cr.repo_id)
                        .target_id(&r.id)
                        .after(&review_json(&r)),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn emit_review(tx: &mut Tx, rec: &CrRecord, kind: &str, r: &Review) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: rec.cr().repo_id.clone(),
            org_id: rec.org_id.clone(),
            body: Some(event::Body::Review(r.clone())),
            subject: Some(bee_proto::v1::SubjectRef {
                kind: SubjectKind::ChangeRequest as i32,
                repo: Some(bee_proto::v1::RepoRef {
                    repo: Some(bee_proto::v1::repo_ref::Repo::Id(rec.cr().repo_id.clone())),
                }),
                id: rec.cr().id.clone(),
            }),
            ..Default::default()
        })
        .await
    }

    async fn insert_thread(
        tx: &mut Tx,
        repo: &Repo,
        rec: &CrRecord,
        a: &Anchor,
        mut first: ReviewComment,
        review_id: &str,
    ) -> Result<(ReviewThread, ReviewComment)> {
        let now = tx.now_ms();
        let tid = tx.new_id()?;
        let cid = tx.new_id()?;
        let cr_id = rec.cr().id.clone();
        let thread = ReviewThread {
            id: tid.clone(),
            change_request_id: cr_id.clone(),
            path: a.path.as_bytes().to_vec(),
            side: a.side.clone(),
            start_line: a.start,
            end_line: a.end,
            revision: a.revision,
            resolved: false,
            resolved_by: None,
            outdated: a.outdated,
            comments: Vec::new(),
        };
        first.id = cid.clone();
        first.thread_id = tid.clone();
        first.created_at = Some(ts(now));
        tx.insert(
            &THREADS,
            &Row::new(repo.id.clone(), tid.clone())
                .k1(cr_id.clone())
                .n1(now as i64)
                .msg(&thread),
        )
        .await?;
        tx.insert(
            &COMMENTS,
            &Row::new(cr_id, cid.clone())
                .k1(tid.clone())
                .n1(now as i64)
                .msg(&CommentRecord {
                    review: Some(first.clone()),
                    review_id: review_id.to_string(),
                    ..Default::default()
                }),
        )
        .await?;
        Self::emit_thread(tx, repo, rec, "review_thread.created", &thread).await?;
        Ok((thread, first))
    }

    async fn insert_reply(
        tx: &mut Tx,
        repo: &Repo,
        rec: &CrRecord,
        thread_id: &str,
        mut c: ReviewComment,
        review_id: &str,
    ) -> Result<ReviewComment> {
        let cr_id = rec.cr().id.clone();
        let Some((thread, _)) = tx
            .get_msg::<ReviewThread>(&THREADS, &repo.id, thread_id)
            .await?
            .filter(|(t, _)| t.change_request_id == cr_id)
        else {
            return Err(ForgeError::not_found(format!("review thread {thread_id}")));
        };
        let last = tx
            .scan(
                &COMMENTS,
                &Scan::scope(cr_id.clone(), 1)
                    .key(Key::K1(thread_id.to_string()))
                    .desc(),
            )
            .await?;
        let now = tx.now_ms();
        let cid = tx.new_id()?;
        c.id = cid.clone();
        c.thread_id = thread_id.to_string();
        if c.in_reply_to.is_empty() {
            c.in_reply_to = last.first().map(|r| r.id.clone()).unwrap_or_default();
        }
        c.path = thread.path.clone();
        c.side = thread.side.clone();
        c.start_line = thread.start_line;
        c.end_line = thread.end_line;
        c.revision = thread.revision;
        c.file_level = thread.start_line == 0;
        c.outdated = thread.outdated;
        c.created_at = Some(ts(now));
        tx.insert(
            &COMMENTS,
            &Row::new(cr_id, cid)
                .k1(thread_id.to_string())
                .n1(now as i64)
                .msg(&CommentRecord {
                    review: Some(c.clone()),
                    review_id: review_id.to_string(),
                    ..Default::default()
                }),
        )
        .await?;
        tx.emit_event(Event {
            kind: "review_comment.created".into(),
            repo_id: repo.id.clone(),
            org_id: rec.org_id.clone(),
            subject: Some(bee_proto::v1::SubjectRef {
                kind: SubjectKind::ChangeRequest as i32,
                repo: repo_ref(repo),
                id: rec.cr().id.clone(),
            }),
            payload_json: serde_json::to_string(&c).unwrap_or_default(),
            ..Default::default()
        })
        .await?;
        Ok(c)
    }

    fn check_suggestion(a: &Anchor, s: &Option<Suggestion>) -> Result<()> {
        if s.is_some() && (a.side != "new" || a.file_level) {
            return Err(ForgeError::invalid(
                "invalid_suggestion",
                "suggestions anchor to lines on the new side",
            ));
        }
        if let Some(s) = s {
            super::store::check_body("suggestion", &s.replacement)?;
        }
        Ok(())
    }

    /// Adds a line, multi-line or file-level comment, starting a review thread. A locked
    /// CR accepts comments only when `moderator`.
    #[allow(clippy::too_many_arguments)]
    pub async fn comment_on_lines(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        body: &str,
        path: &[u8],
        side: &str,
        start: u32,
        end: u32,
        file_level: bool,
        revision: u32,
        suggestion: Option<Suggestion>,
        moderator: bool,
    ) -> Result<ReviewComment> {
        if body.trim().is_empty() && suggestion.is_none() {
            return Err(ForgeError::missing("body"));
        }
        super::store::check_body("body", body)?;
        let (rec, _) = self.record(cx, repo, id).await?;
        require_open(rec.cr())?;
        if rec.cr().locked && !moderator {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Locked,
                "the conversation is locked",
            ));
        }
        let a = self
            .anchor(repo, rec.cr(), path, side, start, end, file_level, revision)
            .await?;
        Self::check_suggestion(&a, &suggestion)?;
        let author = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        let first = ReviewComment {
            path: a.path.as_bytes().to_vec(),
            side: a.side.clone(),
            start_line: a.start,
            end_line: a.end,
            body: body.to_string(),
            revision: a.revision,
            suggestion: suggestion.map(|s| Suggestion {
                replacement: s.replacement,
                applied: false,
                applied_in: None,
            }),
            file_level: a.file_level,
            outdated: a.outdated,
            author: Some(author),
            ..Default::default()
        };
        let (_, c) = Self::insert_thread(&mut tx, repo, &rec, &a, first, "").await?;
        rec.comment_count += 1;
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        tx.audit(
            AuditDraft::new("review.comment", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Comment, &c.id)
                .after(&serde_json::to_string(&c).unwrap_or_default()),
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    /// Replies to a review thread.
    pub async fn reply(
        &self,
        cx: &Cx,
        repo: &Repo,
        thread_id: &str,
        body: &str,
        suggestion: Option<Suggestion>,
        moderator: bool,
    ) -> Result<ReviewComment> {
        if body.trim().is_empty() && suggestion.is_none() {
            return Err(ForgeError::missing("body"));
        }
        super::store::check_body("body", body)?;
        let author = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (thread, _) = tx
            .get_msg::<ReviewThread>(&THREADS, &repo.id, thread_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("review thread {thread_id}")))?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, &thread.change_request_id).await?;
        if rec.cr().locked && !moderator {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Locked,
                "the conversation is locked",
            ));
        }
        if suggestion.is_some() && (thread.side == "old" || thread.start_line == 0) {
            return Err(ForgeError::invalid(
                "invalid_suggestion",
                "suggestions anchor to lines on the new side",
            ));
        }
        let c = ReviewComment {
            body: body.to_string(),
            suggestion,
            author: Some(author),
            ..Default::default()
        };
        let c = Self::insert_reply(&mut tx, repo, &rec, thread_id, c, "").await?;
        rec.comment_count += 1;
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        tx.audit(
            AuditDraft::new("review.reply", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Comment, &c.id)
                .after(&serde_json::to_string(&c).unwrap_or_default()),
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    /// Resolves or unresolves a thread.
    pub async fn resolve_thread(
        &self,
        cx: &Cx,
        repo: &Repo,
        thread_id: &str,
        resolved: bool,
    ) -> Result<ReviewThread> {
        let who = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut t, row) = tx
            .get_msg::<ReviewThread>(&THREADS, &repo.id, thread_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("review thread {thread_id}")))?;
        let (rec, _) = Self::load(&mut tx, &repo.id, &t.change_request_id).await?;
        if t.resolved != resolved {
            t.resolved = resolved;
            t.resolved_by = resolved.then_some(who);
            tx.update(&THREADS, &row.clone().msg(&t)).await?;
            Self::emit_thread(
                &mut tx,
                repo,
                &rec,
                if resolved {
                    "review_thread.resolved"
                } else {
                    "review_thread.unresolved"
                },
                &t,
            )
            .await?;
            tx.audit(
                AuditDraft::new(
                    if resolved {
                        "review.thread_resolve"
                    } else {
                        "review.thread_unresolve"
                    },
                    &rec.org_id,
                )
                .repo_id(&repo.id)
                .target_id(thread_id),
            )
            .await?;
        }
        let comments = Self::thread_comments(&mut tx, &t, &cx.actor_id).await?;
        tx.commit().await?;
        t.comments = comments;
        Ok(t)
    }

    async fn thread_comments(
        tx: &mut Tx,
        t: &ReviewThread,
        viewer: &str,
    ) -> Result<Vec<ReviewComment>> {
        let rows = tx
            .scan_msgs::<CommentRecord>(
                &COMMENTS,
                &Scan::scope(t.change_request_id.clone(), 10_000).key(Key::K1(t.id.clone())),
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c, _)| {
                c.review.map(|mut rc| {
                    rc.path = t.path.clone();
                    rc.side = t.side.clone();
                    rc.start_line = t.start_line;
                    rc.end_line = t.end_line;
                    rc.revision = t.revision;
                    rc.outdated = t.outdated;
                    rc.file_level = t.start_line == 0;
                    rc.reactions = reaction_groups(&c.reactions, viewer);
                    rc
                })
            })
            .collect())
    }

    /// Review threads of a CR with their comments, oldest first, optionally only resolved
    /// or unresolved ones; paged by thread id.
    pub async fn threads(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        resolved: Option<bool>,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<ReviewThread>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load(&mut tx, &repo.id, id).await?;
        let rows = tx
            .scan_msgs::<ReviewThread>(
                &THREADS,
                &Scan::scope(repo.id.clone(), MAX_THREADS)
                    .key(Key::K1(id.to_string()))
                    .after((!token.is_empty()).then(|| Cursor {
                        n1: 0,
                        scope: repo.id.clone(),
                        id: token.to_string(),
                    })),
            )
            .await?;
        let mut out = Vec::new();
        let mut next = String::new();
        for (mut t, row) in rows {
            if resolved.is_some_and(|r| r != t.resolved) {
                continue;
            }
            if out.len() >= limit {
                break;
            }
            t.comments = Self::thread_comments(&mut tx, &t, &cx.actor_id).await?;
            next = row.id.clone();
            out.push(t);
        }
        if out.len() < limit {
            next.clear();
        }
        Ok((out, next))
    }

    // ----- reviews ----------------------------------------------------------------------------

    /// Submits a review of revision `revision` (0 = current) with optional line comments
    /// (each starts a thread, or replies when `in_reply_to` names a comment of a thread).
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_review(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        revision: u32,
        verdict: Verdict,
        body: &str,
        comments: Vec<ReviewComment>,
    ) -> Result<Review> {
        if verdict == Verdict::Unspecified {
            return Err(ForgeError::missing("verdict"));
        }
        super::store::check_body("body", body)?;
        if comments.len() > MAX_REVIEW_COMMENTS {
            return Err(ForgeError::too_large(
                "review_comments",
                MAX_REVIEW_COMMENTS as u64,
                comments.len() as u64,
            ));
        }
        if verdict != Verdict::Approve && body.trim().is_empty() && comments.is_empty() {
            return Err(ForgeError::invalid(
                "missing_field",
                "a review that requests changes or comments needs a body or comments",
            ));
        }
        let (rec, _) = self.record(cx, repo, id).await?;
        require_open(rec.cr())?;
        let current = current_rev(rec.cr());
        let r = if revision == 0 { current } else { revision };
        if r == 0 || r > current {
            return Err(ForgeError::invalid(
                "invalid_revision",
                format!("no revision {revision}"),
            ));
        }
        let author_id = rec
            .cr()
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        if author_id == cx.actor_id && verdict != Verdict::Comment {
            return Err(ForgeError::invalid_state(
                "authors cannot approve or request changes on their own change request",
            ));
        }
        // Validate anchors before the transaction.
        let mut planned = Vec::with_capacity(comments.len());
        for c in comments {
            if !c.in_reply_to.is_empty() {
                planned.push((None, c));
                continue;
            }
            let a = self
                .anchor(
                    repo,
                    rec.cr(),
                    &c.path,
                    &c.side,
                    c.start_line,
                    c.end_line,
                    c.file_level,
                    if c.revision == 0 { r } else { c.revision },
                )
                .await?;
            Self::check_suggestion(&a, &c.suggestion)?;
            planned.push((Some(a), c));
        }
        let reviewer = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        let now = tx.now_ms();
        let rid = tx.new_id()?;
        let mut out_comments = Vec::new();
        for (a, mut c) in planned {
            c.author = Some(reviewer.clone());
            super::store::check_body("body", &c.body)?;
            match a {
                Some(a) => {
                    let (_, c) = Self::insert_thread(&mut tx, repo, &rec, &a, c, &rid).await?;
                    out_comments.push(c);
                }
                None => {
                    let parent = tx
                        .get_msg::<CommentRecord>(&COMMENTS, id, &c.in_reply_to)
                        .await?
                        .and_then(|(p, row)| p.review.map(|_| row.k1.unwrap_or_default()))
                        .ok_or_else(|| {
                            ForgeError::not_found(format!("review comment {}", c.in_reply_to))
                        })?;
                    let c = Self::insert_reply(&mut tx, repo, &rec, &parent, c, &rid).await?;
                    out_comments.push(c);
                }
            }
        }
        rec.comment_count += out_comments.len() as u32;
        let review = Review {
            id: rid.clone(),
            change_id: id.to_string(),
            revision: r,
            reviewer: Some(reviewer.clone()),
            verdict: verdict as i32,
            body: body.to_string(),
            comments: Vec::new(),
            created_at: Some(ts(now)),
            dismissed: false,
            dismiss_reason: String::new(),
            dismissed_by: None,
            stale: r < current,
            counts_toward_policy: false,
        };
        tx.insert(
            &REVIEWS,
            &Row::new(id, rid.clone())
                .k1(cx.actor_id.clone())
                .n1(now as i64)
                .msg(&review),
        )
        .await?;
        // A review answers the reviewer's pending request.
        rec.cr_mut()
            .review_requests
            .retain(|q| q.reviewer.as_ref().is_none_or(|p| p.id != cx.actor_id));
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        let mut served = review.clone();
        served.comments = out_comments;
        Self::emit_review(&mut tx, &rec, "review.submitted", &served).await?;
        tx.audit(
            AuditDraft::new("review.submit", &rec.org_id)
                .repo_id(&repo.id)
                .target_id(&rid)
                .after(&review_json(&review)),
        )
        .await?;
        tx.commit().await?;
        served.counts_toward_policy = self.counts(repo, &rec, &served).await;
        Ok(served)
    }

    /// Whether a review counts toward policy: an approval of the current revision, not
    /// dismissed, not by the author, and by a human unless the land rule's approval rules
    /// include agents.
    pub(crate) async fn counts(&self, repo: &Repo, rec: &CrRecord, r: &Review) -> bool {
        let cr = rec.cr();
        if r.dismissed || verdict_of(r) != Verdict::Approve {
            return false;
        }
        let reviewer = r.reviewer.as_ref().map(|p| p.id.as_str()).unwrap_or("");
        if cr.author.as_ref().is_some_and(|a| a.id == reviewer) {
            return false;
        }
        let policy = self.policy_of(repo).await.ok().flatten();
        let rule = policy
            .as_ref()
            .and_then(|p| super::store::land_rule(p, &cr.target_ref));
        if rule.is_some_and(|b| b.dismiss_stale_approvals) && r.revision != current_rev(cr) {
            return false;
        }
        let human = r
            .reviewer
            .as_ref()
            .is_some_and(|p| p.kind == PrincipalKind::Human as i32 || p.kind == 0);
        human || rule.is_some_and(|b| b.approvals.iter().any(|a| a.include_agents))
    }

    /// Reviews of a CR, oldest first, with their comments.
    pub async fn reviews(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Vec<Review>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, id).await?;
        let rows = tx
            .scan_msgs::<Review>(&REVIEWS, &Scan::scope(id, 10_000))
            .await?;
        let comments = tx
            .scan_msgs::<CommentRecord>(&COMMENTS, &Scan::scope(id, 10_000))
            .await?;
        tx.commit().await?;
        let mut by_review: BTreeMap<String, Vec<ReviewComment>> = BTreeMap::new();
        for (c, _) in comments {
            if let Some(rc) = c.review
                && !c.review_id.is_empty()
            {
                by_review.entry(c.review_id).or_default().push(rc);
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        for (mut r, _) in rows {
            r.comments = by_review.remove(&r.id).unwrap_or_default();
            r.counts_toward_policy = self.counts(repo, &rec, &r).await;
            out.push(r);
        }
        Ok(out)
    }

    /// Dismisses a review so it no longer counts toward policy.
    pub async fn dismiss_review(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        review_id: &str,
        reason: &str,
    ) -> Result<Review> {
        if reason.trim().is_empty() {
            return Err(ForgeError::missing("reason"));
        }
        super::store::check_body("reason", reason)?;
        let by = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        super::store::lock_change_request(&mut tx, &repo.id, id).await?;
        let (mut rec, cr_row) = Self::load(&mut tx, &repo.id, id).await?;
        let expired_intent = rec.clear_expired_landing_intent(tx.now_ms())?;
        require_open(rec.cr())?;
        let (mut r, row) = tx
            .get_msg::<Review>(&REVIEWS, id, review_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("review {review_id}")))?;
        if r.dismissed {
            return Err(ForgeError::invalid_state("the review is already dismissed"));
        }
        if verdict_of(&r) == Verdict::Comment {
            return Err(ForgeError::invalid_state(
                "comment reviews cannot be dismissed",
            ));
        }
        r.dismissed = true;
        r.dismiss_reason = reason.to_string();
        r.dismissed_by = Some(by);
        tx.update(&REVIEWS, &row.clone().msg(&r)).await?;
        if expired_intent {
            let (updated, _) = Changes::save(&mut tx, rec, &cr_row).await?;
            rec = updated;
        }
        Self::emit_review(&mut tx, &rec, "review.dismissed", &r).await?;
        tx.audit(
            AuditDraft::new("review.dismiss", &rec.org_id)
                .repo_id(&repo.id)
                .target_id(review_id)
                .after(&review_json(&r)),
        )
        .await?;
        tx.commit().await?;
        r.counts_toward_policy = false;
        Ok(r)
    }

    // ----- suggestions -------------------------------------------------------------------------

    /// Applies suggestions as one new changeset on top of the CR's head, authored by the
    /// caller (provenance: the caller's delegation chain), and records it as a new revision.
    pub async fn apply_suggestions(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        comment_ids: &[String],
        message: &str,
    ) -> Result<ChangeRequest> {
        if comment_ids.is_empty() {
            return Err(ForgeError::missing("comment_ids"));
        }
        if comment_ids.len() > MAX_REVIEW_COMMENTS {
            return Err(ForgeError::too_large(
                "suggestions",
                MAX_REVIEW_COMMENTS as u64,
                comment_ids.len() as u64,
            ));
        }
        let (rec, _) = self.record(cx, repo, id).await?;
        require_open(rec.cr())?;
        let current = current_rev(rec.cr());
        let head = head_of(rec.cr())
            .cloned()
            .ok_or_else(|| ForgeError::internal("CR without revision"))?;
        let mut by_path: BTreeMap<String, Vec<SuggestionEdit>> = BTreeMap::new();
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let mut seen = BTreeSet::new();
            for cid in comment_ids {
                if !seen.insert(cid.clone()) {
                    continue;
                }
                let (c, row) = tx
                    .get_msg::<CommentRecord>(&COMMENTS, id, cid)
                    .await?
                    .ok_or_else(|| ForgeError::not_found(format!("review comment {cid}")))?;
                let rc = c.review.ok_or_else(|| {
                    ForgeError::invalid(
                        "invalid_suggestion",
                        format!("{cid} is not a review comment"),
                    )
                })?;
                let s = rc.suggestion.clone().ok_or_else(|| {
                    ForgeError::invalid("invalid_suggestion", format!("{cid} has no suggestion"))
                })?;
                if s.applied {
                    return Err(ForgeError::invalid_state(format!(
                        "the suggestion in {cid} is already applied"
                    )));
                }
                let tid = row.k1.clone().unwrap_or_default();
                let (t, _) = tx
                    .get_msg::<ReviewThread>(&THREADS, &repo.id, &tid)
                    .await?
                    .ok_or_else(|| ForgeError::not_found(format!("review thread {tid}")))?;
                if t.outdated || t.revision != current {
                    return Err(ForgeError::invalid_state(format!(
                        "the suggestion in {cid} is outdated"
                    )));
                }
                by_path
                    .entry(String::from_utf8_lossy(&t.path).into_owned())
                    .or_default()
                    .push(SuggestionEdit {
                        start: t.start_line,
                        end: t.end_line,
                        replacement: s.replacement,
                    });
            }
        }
        let mut edits = Vec::new();
        for (path, list) in &by_path {
            let content = self
                .vcs
                .read_file(repo, &head, path)
                .await?
                .ok_or_else(|| {
                    ForgeError::invalid_state(format!("{path} is gone from the head"))
                })?;
            edits.push((path.clone(), Some(text::apply_suggestions(&content, list)?)));
        }
        let actor = self.actor(cx).await;
        let message = if message.trim().is_empty() {
            "Apply suggestions from code review".to_string()
        } else {
            message.to_string()
        };
        let new_head = self
            .vcs
            .create_commit(
                repo,
                NewCommit {
                    parent: Some(head),
                    edits,
                    message,
                    author: if actor.display.is_empty() {
                        actor.id.clone()
                    } else {
                        actor.display.clone()
                    },
                    creator: cx.actor_id.clone(),
                    chain: cx.chain.clone(),
                    change_id: None,
                },
            )
            .await?;
        self.revise(cx, repo, id, &new_head).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, id).await?;
        for cid in comment_ids {
            if let Some((mut c, row)) = tx.get_msg::<CommentRecord>(&COMMENTS, id, cid).await?
                && let Some(rc) = c.review.as_mut()
                && let Some(s) = rc.suggestion.as_mut()
                && !s.applied
            {
                s.applied = true;
                s.applied_in = Some(new_head.clone());
                tx.update(&COMMENTS, &row.clone().msg(&c)).await?;
            }
        }
        tx.audit(
            Self::audit_cr("cr.apply_suggestions", &rec).before(
                &serde_json::json!({
                    "comment_ids": comment_ids,
                    "changeset": hex(&new_head.raw),
                })
                .to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        self.get(cx, repo, id).await
    }

    // ----- viewed files ------------------------------------------------------------------------

    /// Marks a file viewed (or not) for the caller on the current revision.
    pub async fn set_viewed(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        path: &[u8],
        viewed: bool,
    ) -> Result<()> {
        let p = std::str::from_utf8(path)
            .map_err(|_| ForgeError::invalid("invalid_path", "paths must be UTF-8"))?
            .trim_matches('/')
            .to_string();
        if p.is_empty() {
            return Err(ForgeError::missing("path"));
        }
        let (rec, _) = self.record(cx, repo, id).await?;
        let head = head_of(rec.cr()).cloned().unwrap_or_default();
        let blob = if viewed {
            self.vcs.file_id(repo, &head, &p).await?
        } else {
            None
        };
        let key = format!("{}:{}", cx.actor_id, hex(p.as_bytes()));
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, id).await?;
        if viewed {
            tx.put(
                &VIEWED,
                &Row::new(id, key)
                    .k1(cx.actor_id.clone())
                    .msg(&ViewedRecord {
                        path: p.as_bytes().to_vec(),
                        revision: current_rev(rec.cr()),
                        blob,
                    }),
            )
            .await?;
        } else {
            tx.delete(&VIEWED, id, &key).await?;
        }
        tx.audit(
            AuditDraft::new(
                if viewed {
                    "cr.view_file"
                } else {
                    "cr.unview_file"
                },
                &rec.org_id,
            )
            .repo_id(&repo.id)
            .target(SubjectKind::ChangeRequest, id)
            .after(&serde_json::json!({ "path": p }).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// The caller's viewed files, with whether each changed since it was viewed.
    pub async fn viewed(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Vec<ViewedFile>> {
        let (rec, _) = self.record(cx, repo, id).await?;
        let head = head_of(rec.cr()).cloned().unwrap_or_default();
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<ViewedRecord>(
                &VIEWED,
                &Scan::scope(id, 10_000).key(Key::K1(cx.actor_id.clone())),
            )
            .await?
        };
        let mut out = Vec::with_capacity(rows.len());
        for (v, _) in rows {
            let path = String::from_utf8_lossy(&v.path).into_owned();
            let now = self.vcs.file_id(repo, &head, &path).await?;
            out.push(ViewedFile {
                changed_since: now != v.blob,
                path: v.path,
                revision: v.revision,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    // ----- revision diffs ------------------------------------------------------------------------

    /// The diff between two revisions ("changes since my review"). `from` 0 = the revision
    /// the caller last reviewed (the CR's base when there is none); `to` 0 = current. When the
    /// revisions have different bases, only paths either revision changes are compared, so
    /// upstream changes picked up by a rebase do not show.
    #[allow(clippy::too_many_arguments)]
    pub async fn diff_revisions(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        from: u32,
        to: u32,
        hunks: bool,
        context: u32,
    ) -> Result<Vec<DiffItem>> {
        self.diff_revisions_filtered(
            cx,
            repo,
            id,
            from,
            to,
            hunks,
            context,
            &DiffFilter::default(),
        )
        .await
    }

    /// [`Changes::diff_revisions`] limited to `filter.paths` (and the paths under them), with
    /// whitespace-only changes left out when `filter.ignore_whitespace`.
    #[allow(clippy::too_many_arguments)]
    pub async fn diff_revisions_filtered(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        from: u32,
        to: u32,
        hunks: bool,
        context: u32,
        filter: &DiffFilter,
    ) -> Result<Vec<DiffItem>> {
        for p in &filter.paths {
            if p.is_empty() || p.starts_with('/') || p.split('/').any(|s| s == ".." || s == ".") {
                return Err(ForgeError::invalid(
                    "invalid_path",
                    format!("invalid path filter {p:?}"),
                ));
            }
        }
        let ws = filter.ignore_whitespace;
        let (rec, _) = self.record(cx, repo, id).await?;
        let cr = rec.cr();
        let current = current_rev(cr);
        let to_n = if to == 0 { current } else { to };
        let to_rev = cr
            .revisions
            .get(to_n.saturating_sub(1) as usize)
            .filter(|_| to_n >= 1)
            .ok_or_else(|| ForgeError::invalid("invalid_revision", format!("no revision {to}")))?
            .clone();
        let from_n = if from == 0 {
            self.reviews(cx, repo, id)
                .await?
                .iter()
                .filter(|r| r.reviewer.as_ref().is_some_and(|p| p.id == cx.actor_id))
                .map(|r| r.revision)
                .max()
                .unwrap_or(0)
        } else {
            from
        };
        if from_n > current {
            return Err(ForgeError::invalid(
                "invalid_revision",
                format!("no revision {from}"),
            ));
        }
        let to_head = to_rev.head.clone().unwrap_or_default();
        let classes: BTreeMap<&str, &str> = rec
            .paths
            .iter()
            .map(|p| (p.path.as_str(), p.class.as_str()))
            .collect();
        let mut items = if from_n == 0 {
            let base = to_rev.base.clone().unwrap_or_default();
            self.vcs
                .diff_with(repo, &base, &to_head, &filter.paths, hunks, context, ws)
                .await?
        } else {
            if from_n == to_n {
                return Ok(Vec::new());
            }
            let from_rev = cr.revisions[from_n as usize - 1].clone();
            let from_head = from_rev.head.clone().unwrap_or_default();
            if from_rev.base == to_rev.base {
                self.vcs
                    .diff_with(
                        repo,
                        &from_head,
                        &to_head,
                        &filter.paths,
                        hunks,
                        context,
                        ws,
                    )
                    .await?
            } else {
                let mut paths = BTreeSet::new();
                for (b, h) in [
                    (from_rev.base.clone().unwrap_or_default(), from_head.clone()),
                    (to_rev.base.clone().unwrap_or_default(), to_head.clone()),
                ] {
                    for d in self.vcs.diff(repo, &b, &h, &filter.paths, false, 0).await? {
                        paths.insert(String::from_utf8_lossy(&d.path).into_owned());
                        if !d.old_path.is_empty() {
                            paths.insert(String::from_utf8_lossy(&d.old_path).into_owned());
                        }
                    }
                }
                if paths.is_empty() {
                    return Ok(Vec::new());
                }
                let paths: Vec<String> = paths.into_iter().collect();
                self.vcs
                    .diff_with(repo, &from_head, &to_head, &paths, hunks, context, ws)
                    .await?
            }
        };
        if to_n == current {
            for it in &mut items {
                let p = String::from_utf8_lossy(&it.path).into_owned();
                if let Some(c) = classes.get(p.as_str()) {
                    it.diff_class = c.to_string();
                }
            }
        }
        Ok(items)
    }

    /// Ids of unresolved threads on a CR.
    pub(crate) async fn unresolved_threads(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<Vec<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<ReviewThread>(
                &THREADS,
                &Scan::scope(repo.id.clone(), MAX_THREADS).key(Key::K1(id.to_string())),
            )
            .await?
            .into_iter()
            .filter(|(t, _)| !t.resolved)
            .map(|(t, _)| t.id)
            .collect())
    }

    /// Reviews in chronological order (for land decisions and queries).
    pub(crate) async fn raw_reviews(&self, cx: &Cx, id: &str) -> Result<Vec<Review>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<Review>(&REVIEWS, &Scan::scope(id, 10_000))
            .await?
            .into_iter()
            .map(|(r, _)| r)
            .collect())
    }
}
