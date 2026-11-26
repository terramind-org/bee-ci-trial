//! GitHub records → sink records ([`crate::sink`]).
//!
//! - Authors become placeholder principals (or their claimants).
//! - Text is link-rewritten ([`crate::links::rewrite`]) with the original kept.
//! - PR revisions come from the force-push timeline (SPEC §17.2): each force-push's `before`
//!   head closes a revision, and the current head is the last one. A `before` commit the
//!   mirror could not fetch is a [`reason::FORCE_PUSH_COMMIT_UNREACHABLE`] drop.
//! - Reviews, review comments and conversation comments are merged into one time-ordered
//!   event list; pending reviews are excluded (they were never visible).

use std::collections::BTreeSet;

use crate::links::rewrite;
use crate::model::{Issue, Metadata, PullRequest, Reaction, User};
use crate::principals::Directory;
use crate::report::{Outcome, category, reason};
use crate::sink::{
    Author, ChangeEvent, ChangeImport, CommentImport, IssueImport, PrState, ReactionImport,
    ReviewState, RevisionImport, Text,
};

/// Link rewriting context.
#[derive(Clone, Debug)]
pub struct Links {
    /// `https://github.com/owner/repo`.
    pub web_repo: String,
    /// `https://hub.example/org/repo`.
    pub hub_repo: String,
}

impl Links {
    /// Rewritten text.
    pub fn text(&self, s: &str) -> Text {
        Text {
            body: rewrite(s, &self.web_repo, &self.hub_repo),
            original: s.to_string(),
        }
    }
}

/// The author record of an account (`None` → the ghost).
pub fn author(dir: &mut Directory, user: &Option<User>) -> Author {
    let u = user.clone().unwrap_or_else(User::ghost);
    let p = dir.of(&u);
    Author {
        principal: dir.acting(u.id).unwrap_or(p.principal),
        github_id: u.id,
        login: u.login,
        agent: p.agent,
    }
}

fn reactions(meta: &Metadata, subject: &str, dir: &mut Directory) -> Vec<ReactionImport> {
    meta.reactions
        .get(subject)
        .map(|m| {
            m.values()
                .map(|r: &Reaction| ReactionImport {
                    id: r.id,
                    author: author(dir, &r.user),
                    content: r.content.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The bee name of a branch.
pub fn branch_ref(branch: &str) -> String {
    bee_import::bee_ref_name(format!("refs/heads/{branch}").as_bytes())
        .map(|(n, _)| n)
        .unwrap_or_else(|| branch.to_string())
}

fn push_head(
    revisions: &mut Vec<RevisionImport>,
    head: &str,
    pusher: Option<Author>,
    at: &str,
    forced: bool,
) {
    if revisions.last().is_some_and(|r| r.head == head) {
        return;
    }
    revisions.push(RevisionImport {
        head: head.to_string(),
        pusher,
        at: at.to_string(),
        forced,
    });
}

/// What the importer knows about the git side of a PR.
#[derive(Clone, Debug, Default)]
pub struct PrGit {
    /// Commits present in the imported history (heads and `before` commits checked).
    pub present: BTreeSet<String>,
    /// The PR has a `pull/<n>/base` ref.
    pub base_ref: bool,
}

/// A PR as a change-request record, plus the outcomes decided before the sink sees it
/// (unreachable force-push commits, excluded pending reviews). `None` (with a drop) when
/// the PR's head commit is not in the imported history at all.
pub fn change(
    meta: &Metadata,
    pr: &PullRequest,
    git: &PrGit,
    dir: &mut Directory,
    links: &Links,
) -> (Option<ChangeImport>, Vec<Outcome>) {
    let n = pr.number;
    let mut pre = Vec::new();
    let pr_author = author(dir, &pr.user);
    let by_login = |login: &str| meta.users.values().find(|u| u.login == login).cloned();
    // Revisions.
    let mut revisions: Vec<RevisionImport> = Vec::new();
    for (i, fp) in meta.force_pushes.get(&n).into_iter().flatten().enumerate() {
        let item = format!("pull/{n}@force-push-{}", i + 1);
        let pusher = fp
            .actor
            .as_deref()
            .and_then(by_login)
            .map(|u| author(dir, &Some(u)));
        match &fp.before {
            Some(sha) if git.present.contains(sha) => {
                pre.push(Outcome::imported(category::FORCE_PUSH_COMMITS, item));
                push_head(&mut revisions, sha, if i == 0 { Some(pr_author.clone()) } else { pusher }, &fp.at, i > 0);
            }
            Some(sha) => pre.push(Outcome::dropped(
                category::FORCE_PUSH_COMMITS,
                item,
                reason::FORCE_PUSH_COMMIT_UNREACHABLE,
                format!("the head {sha} before this force-push is no longer served by GitHub; the revision it ended is lost"),
            )),
            None => pre.push(Outcome::dropped(
                category::FORCE_PUSH_COMMITS,
                item,
                reason::FORCE_PUSH_COMMIT_UNREACHABLE,
                "GitHub no longer knows the head before this force-push".to_string(),
            )),
        }
    }
    let last_forced = meta.force_pushes.get(&n).is_some_and(|v| !v.is_empty());
    if !git.present.contains(&pr.head.sha) {
        pre.push(Outcome::dropped(
            category::PULL_REQUESTS,
            format!("pull/{n}"),
            reason::MISSING_COMMIT,
            format!(
                "the head {} is not in the imported history (GitHub removed refs/pull/{n}/head)",
                pr.head.sha
            ),
        ));
        return (None, pre);
    }
    let last_push = meta.force_pushes.get(&n).and_then(|v| v.last());
    let last_pusher = last_push
        .and_then(|f| f.actor.as_deref())
        .and_then(by_login)
        .map(|u| author(dir, &Some(u)));
    let none_yet = revisions.is_empty();
    push_head(
        &mut revisions,
        &pr.head.sha,
        if none_yet {
            Some(pr_author.clone())
        } else {
            last_pusher
        },
        last_push.map(|f| f.at.as_str()).unwrap_or(""),
        last_forced && !none_yet,
    );
    // Events.
    let mut events = Vec::new();
    for r in meta.reviews.get(&n).into_iter().flat_map(|m| m.values()) {
        let state = match r.state.as_str() {
            "APPROVED" => ReviewState::Approved,
            "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
            "DISMISSED" => ReviewState::Dismissed,
            "PENDING" => {
                pre.push(Outcome::excluded(
                    category::REVIEWS,
                    format!("pull/{n}#review-{}", r.id),
                    "a pending review was never submitted or visible",
                ));
                continue;
            }
            _ => ReviewState::Commented,
        };
        events.push(ChangeEvent::Review {
            id: r.id,
            author: author(dir, &r.user),
            state,
            body: links.text(r.body.as_deref().unwrap_or("")),
            commit: r.commit_id.clone(),
            at: r.submitted_at.clone().unwrap_or_default(),
        });
    }
    for c in meta
        .review_comments
        .values()
        .filter(|c| c.pr_number() == Some(n))
    {
        let file_level = c.subject_type.as_deref() == Some("file")
            || (c.original_line.is_none() && c.line.is_none());
        events.push(ChangeEvent::ReviewComment {
            id: c.id,
            author: author(dir, &c.user),
            body: links.text(&c.body),
            path: c.path.clone(),
            side: match c.side.as_deref() {
                Some("LEFT") => "old".into(),
                _ => "new".into(),
            },
            start_line: c.original_start_line.or(c.start_line),
            line: c.original_line.or(c.line),
            file_level,
            commit: if c.original_commit_id.is_empty() {
                c.commit_id.clone()
            } else {
                c.original_commit_id.clone()
            },
            diff_hunk: c.diff_hunk.clone(),
            in_reply_to: c.in_reply_to_id,
            review: c.pull_request_review_id,
            at: c.created_at.clone(),
            reactions: reactions(meta, &format!("review_comment:{}", c.id), dir),
        });
    }
    for c in meta
        .issue_comments
        .values()
        .filter(|c| c.number() == Some(n))
    {
        events.push(ChangeEvent::Comment {
            id: c.id,
            author: author(dir, &c.user),
            body: links.text(&c.body),
            at: c.created_at.clone(),
            reactions: reactions(meta, &format!("issue_comment:{}", c.id), dir),
        });
    }
    // Time order; replies after what they reply to even when clocks tie.
    events.sort_by(|a, b| (a.at(), a.id()).cmp(&(b.at(), b.id())));
    let depends_on = meta
        .pulls
        .values()
        .find(|o| {
            o.number != n
                && o.state == "open"
                && pr.state == "open"
                && o.head.branch == pr.base.branch
                && o.head.repo.as_ref().map(|r| &r.full_name)
                    == pr.base.repo.as_ref().map(|r| &r.full_name)
        })
        .map(|o| o.number);
    let state = if pr.merged() {
        PrState::Merged {
            merge_commit: pr.merge_commit_sha.clone(),
        }
    } else if pr.state == "closed" {
        PrState::Closed
    } else {
        PrState::Open
    };
    let c = ChangeImport {
        number: n,
        title: pr.title.clone(),
        body: links.text(pr.body.as_deref().unwrap_or("")),
        author: pr_author,
        state,
        draft: pr.draft,
        locked: pr.locked,
        target_ref: branch_ref(&pr.base.branch),
        base_ref: git.base_ref.then(|| format!("pull/{n}/base")),
        depends_on,
        revisions,
        labels: pr.labels.iter().map(|l| l.name.clone()).collect(),
        assignees: pr
            .assignees
            .iter()
            .map(|u| author(dir, &Some(u.clone())))
            .collect(),
        milestone: pr.milestone.as_ref().map(|m| m.number),
        reviewers: pr
            .requested_reviewers
            .iter()
            .map(|u| author(dir, &Some(u.clone())))
            .collect(),
        events,
        reactions: reactions(meta, &format!("issue:{n}"), dir),
        created_at: pr.created_at.clone(),
        closed_at: pr.closed_at.clone(),
        url: format!("{}/pull/{n}", links.web_repo),
    };
    (Some(c), pre)
}

/// An issue as a sink record.
pub fn issue(meta: &Metadata, i: &Issue, dir: &mut Directory, links: &Links) -> IssueImport {
    let n = i.number;
    let mut comments: Vec<CommentImport> = meta
        .issue_comments
        .values()
        .filter(|c| c.number() == Some(n))
        .map(|c| CommentImport {
            id: c.id,
            author: author(dir, &c.user),
            body: links.text(&c.body),
            at: c.created_at.clone(),
            reactions: reactions(meta, &format!("issue_comment:{}", c.id), dir),
        })
        .collect();
    comments.sort_by(|a, b| (&a.at, a.id).cmp(&(&b.at, b.id)));
    IssueImport {
        number: n,
        title: i.title.clone(),
        body: links.text(i.body.as_deref().unwrap_or("")),
        author: author(dir, &i.user),
        state: i.state.clone(),
        state_reason: i.state_reason.clone(),
        labels: i.labels.iter().map(|l| l.name.clone()).collect(),
        assignees: i
            .assignees
            .iter()
            .map(|u| author(dir, &Some(u.clone())))
            .collect(),
        milestone: i.milestone.as_ref().map(|m| m.number),
        locked: i.locked,
        comments,
        reactions: reactions(meta, &format!("issue:{n}"), dir),
        created_at: i.created_at.clone(),
        closed_at: i.closed_at.clone(),
    }
}

/// A stable fingerprint of a record (to place changed records again).
pub fn fingerprint<T: serde::Serialize>(v: &T) -> String {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    crate::principals::hex(&blake3::hash(&bytes).as_bytes()[..16])
}

/// Contributions per GitHub account across the records (for the report).
pub fn contributions(meta: &Metadata) -> std::collections::BTreeMap<u64, u64> {
    let mut m = std::collections::BTreeMap::new();
    let mut add = |u: &Option<User>| {
        *m.entry(u.as_ref().map(|u| u.id).unwrap_or(crate::model::GHOST_ID))
            .or_insert(0) += 1;
    };
    meta.pulls.values().for_each(|p| add(&p.user));
    meta.issues.values().for_each(|i| add(&i.user));
    meta.reviews
        .values()
        .flat_map(|r| r.values())
        .for_each(|r| add(&r.user));
    meta.review_comments.values().for_each(|c| add(&c.user));
    meta.issue_comments.values().for_each(|c| add(&c.user));
    meta.reactions
        .values()
        .flat_map(|r| r.values())
        .for_each(|r| add(&r.user));
    meta.releases.values().for_each(|r| add(&r.author));
    m
}
