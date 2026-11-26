//! `ListChanges`: request filters plus the GitHub-compatible query language
//! (`is:open review:required label:bug author:@me path:src risk:>=logic sort:updated-asc`),
//! parsed by `bee_search::parse_forge_query` and evaluated against CR rows here, so lists
//! never depend on a search index (PRODUCT §15.1).
//!
//! Qualifiers that name something CRs do not have (projects, languages, commit fields)
//! match nothing; `type:issue` and `is:issue` match nothing, `type:cr` everything.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use bee_proto::v1::{ChangeRequest, ChangeState, Repo, Verdict};
use bee_search::forge::filter::{CompiledFilter, Filter, FilterContext, Predicate, SortOrder};
use bee_search::forge::{CivilDate, ForgeKind, Key, RISK_CLASSES};

use super::landing::ms;
use super::model::CrRecord;
use super::store::{Changes, head_of, hex_of};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};

/// The request-level filters of `ListChanges`.
#[derive(Clone, Debug, Default)]
pub struct ListFilter {
    /// State (unspecified = any).
    pub state: ChangeState,
    /// Author.
    pub author_id: String,
    /// Target ref.
    pub target_ref: String,
    /// Every one of these labels.
    pub labels: Vec<String>,
    /// A pending review request for this principal or team.
    pub review_requested_id: String,
    /// Assignee.
    pub assignee_id: String,
    /// Draft or not.
    pub draft: Option<bool>,
    /// Touches paths under this prefix.
    pub path_prefix: String,
    /// GitHub-compatible query.
    pub query: String,
}

/// Facts about one CR that predicates read, gathered on demand.
#[derive(Default)]
struct Facts {
    reviewers: BTreeSet<String>,
    decision: &'static str,
    commenters: BTreeSet<String>,
    status: &'static str,
}

#[derive(Default)]
struct Needs {
    reviews: bool,
    commenters: bool,
    status: bool,
}

fn walk(f: &Filter, n: &mut Needs) {
    match f {
        Filter::All => {}
        Filter::Pred(p) => match p {
            Predicate::Person { role, .. } => match role {
                Key::ReviewedBy => n.reviews = true,
                Key::Commenter => n.commenters = true,
                Key::Involves => {
                    n.reviews = true;
                    n.commenters = true;
                }
                _ => {}
            },
            Predicate::Enum {
                field: "review", ..
            } => n.reviews = true,
            Predicate::Enum {
                field: "status", ..
            } => n.status = true,
            _ => {}
        },
        Filter::Not(x) => walk(x, n),
        Filter::And(v) | Filter::Or(v) => v.iter().for_each(|x| walk(x, n)),
    }
}

fn in_bounds(v: i64, min: &Bound<i64>, max: &Bound<i64>) -> bool {
    let lo = match min {
        Bound::Included(m) => v >= *m,
        Bound::Excluded(m) => v > *m,
        Bound::Unbounded => true,
    };
    let hi = match max {
        Bound::Included(m) => v <= *m,
        Bound::Excluded(m) => v < *m,
        Bound::Unbounded => true,
    };
    lo && hi
}

fn ci_contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

struct Eval<'a> {
    repo: &'a Repo,
    ids: &'a BTreeMap<String, BTreeSet<String>>,
}

impl Eval<'_> {
    fn who(&self, login: &str) -> BTreeSet<String> {
        self.ids
            .get(login)
            .cloned()
            .unwrap_or_else(|| BTreeSet::from([login.to_string()]))
    }

    fn matches(&self, f: &Filter, rec: &CrRecord, facts: &Facts) -> bool {
        match f {
            Filter::All => true,
            Filter::Pred(p) => self.pred(p, rec, facts),
            Filter::Not(x) => !self.matches(x, rec, facts),
            Filter::And(v) => v.iter().all(|x| self.matches(x, rec, facts)),
            Filter::Or(v) => v.iter().any(|x| self.matches(x, rec, facts)),
        }
    }

    fn pred(&self, p: &Predicate, rec: &CrRecord, facts: &Facts) -> bool {
        let cr = rec.cr();
        let author = cr.author.as_ref().map(|a| a.id.as_str()).unwrap_or("");
        match p {
            Predicate::Text { text, fields } => fields.iter().any(|f| match *f {
                "title" => ci_contains(&cr.title, text),
                "body" => ci_contains(&cr.body, text) || ci_contains(&cr.summary, text),
                _ => false,
            }),
            Predicate::Person { role, login } => {
                let ids = self.who(login);
                let assigned = cr.assignees.iter().any(|a| ids.contains(&a.id));
                match role {
                    Key::Author => ids.contains(author),
                    Key::Assignee => assigned,
                    Key::Commenter => facts.commenters.iter().any(|c| ids.contains(c)),
                    Key::ReviewedBy => facts.reviewers.iter().any(|c| ids.contains(c)),
                    Key::ReviewRequested => cr.review_requests.iter().any(|r| {
                        r.reviewer.as_ref().is_some_and(|p| ids.contains(&p.id))
                            || (!r.team_id.is_empty() && ids.contains(&r.team_id))
                    }),
                    Key::Mentions => ids.iter().any(|i| {
                        cr.body.contains(&format!("@{i}")) || cr.body.contains(&format!("@{login}"))
                    }),
                    Key::Involves => {
                        ids.contains(author)
                            || assigned
                            || facts.commenters.iter().any(|c| ids.contains(c))
                            || facts.reviewers.iter().any(|c| ids.contains(c))
                    }
                    _ => false,
                }
            }
            Predicate::AgentAuthor { is_agent, login } => {
                let agent = cr
                    .author
                    .as_ref()
                    .is_some_and(|a| a.kind == bee_proto::v1::PrincipalKind::Agent as i32);
                match login {
                    Some(l) => agent && self.who(l).contains(author),
                    None => agent == *is_agent,
                }
            }
            Predicate::Name { field, value } => match field {
                Key::Label => cr.labels.iter().any(|l| l.name.eq_ignore_ascii_case(value)),
                Key::Milestone => cr.milestone.as_ref().is_some_and(|m| {
                    m.id.eq_ignore_ascii_case(value) || m.title.eq_ignore_ascii_case(value)
                }),
                Key::Base => cr.target_ref.eq_ignore_ascii_case(value),
                Key::Head => {
                    head_of(cr).is_some_and(|h| hex_of(h).starts_with(&value.to_lowercase()))
                }
                Key::Repo => self.repo.full_name.eq_ignore_ascii_case(value),
                Key::Org | Key::User => self
                    .repo
                    .full_name
                    .split('/')
                    .next()
                    .is_some_and(|o| o.eq_ignore_ascii_case(value)),
                Key::TeamReviewRequested => cr.review_requests.iter().any(|r| {
                    !r.team_id.is_empty()
                        && (r.team_id.eq_ignore_ascii_case(value)
                            || value
                                .rsplit('/')
                                .next()
                                .is_some_and(|t| r.team_id.eq_ignore_ascii_case(t)))
                }),
                _ => false,
            },
            Predicate::PathPrefix(prefix) => rec
                .paths
                .iter()
                .any(|p| crate::leases::covers(prefix.trim_end_matches('/'), &p.path)),
            Predicate::Enum { field, value } => match *field {
                "state" => match *value {
                    "open" => cr.state == ChangeState::Open as i32,
                    "closed" => cr.state != ChangeState::Open as i32,
                    "merged" => cr.state == ChangeState::Landed as i32,
                    _ => false,
                },
                "type" => *value == "cr",
                "review" => match *value {
                    "none" => facts.decision == "none",
                    "approved" => facts.decision == "approved",
                    "changes_requested" => facts.decision == "changes_requested",
                    "required" => {
                        cr.state == ChangeState::Open as i32 && facts.decision != "approved"
                    }
                    _ => false,
                },
                "status" => facts.status == *value,
                "linked" => *value == "issue" && !rec.linked_issues().is_empty(),
                "visibility" => {
                    let v = bee_proto::v1::Visibility::try_from(self.repo.visibility)
                        .unwrap_or(bee_proto::v1::Visibility::Private);
                    v.as_str_name()
                        .trim_start_matches("VISIBILITY_")
                        .eq_ignore_ascii_case(value)
                }
                _ => false,
            },
            Predicate::Flag { field, value } => {
                let actual = match *field {
                    "draft" => cr.draft,
                    "locked" => cr.locked,
                    "queued" => cr.merge_queue_entry.is_some(),
                    "archived" => self.repo.archived,
                    _ => false,
                };
                actual == *value
            }
            Predicate::Range { field, min, max } => {
                let v: Option<i64> = match field {
                    Key::Created => Some(ms(&cr.created_at) as i64),
                    Key::Updated => Some(ms(&cr.updated_at) as i64),
                    Key::Closed => cr.closed_at.or(cr.landed_at).map(|t| ms(&Some(t)) as i64),
                    Key::Merged => cr.landed_at.map(|t| ms(&Some(t)) as i64),
                    Key::Comments => Some(i64::from(rec.comment_count)),
                    Key::Reactions => Some(rec.reactions.len() as i64),
                    Key::Interactions => {
                        Some(i64::from(rec.comment_count) + rec.reactions.len() as i64)
                    }
                    Key::Risk => cr
                        .revisions
                        .last()
                        .and_then(|r| RISK_CLASSES.iter().position(|c| *c == r.diff_class))
                        .map(|i| i as i64),
                    _ => None,
                };
                v.is_some_and(|v| in_bounds(v, min, max))
            }
            Predicate::Missing(w) => match *w {
                "label" => cr.labels.is_empty(),
                "milestone" => cr.milestone.is_none(),
                "assignee" => cr.assignees.is_empty(),
                "reviewer" => cr.review_requests.is_empty() && facts.reviewers.is_empty(),
                "project" => true,
                _ => false,
            },
        }
    }
}

/// Parses and compiles a CR query for `me` at `now_ms`.
pub fn compile(query: &str, me: &str, now_ms: u64) -> Result<CompiledFilter> {
    let q = bee_search::parse_forge_query(ForgeKind::Issues, query).map_err(|errs| {
        let msg = errs
            .iter()
            .map(|e| format!("{} at {}..{}", e.message, e.span.start, e.span.end))
            .collect::<Vec<_>>()
            .join("; ");
        ForgeError::invalid(errs.first().map(|e| e.subcode).unwrap_or("bad_query"), msg)
    })?;
    q.to_filter(&FilterContext {
        me: Some(me.to_string()),
        today: Some(CivilDate::from_days((now_ms / 86_400_000) as i64)),
    })
    .map_err(|e| ForgeError::invalid("bad_query", e.message))
}

impl Changes {
    async fn facts(&self, cx: &Cx, repo: &Repo, rec: &CrRecord, needs: &Needs) -> Result<Facts> {
        let cr = rec.cr();
        let mut f = Facts {
            decision: "none",
            status: "pending",
            ..Facts::default()
        };
        if needs.reviews {
            let mut last: BTreeMap<String, i32> = BTreeMap::new();
            for r in self.raw_reviews(cx, &cr.id).await? {
                let Some(p) = &r.reviewer else { continue };
                f.reviewers.insert(p.id.clone());
                if !r.dismissed && r.verdict != Verdict::Comment as i32 {
                    last.insert(p.id.clone(), r.verdict);
                }
            }
            f.decision = if last.values().any(|v| *v == Verdict::RequestChanges as i32) {
                "changes_requested"
            } else if last.values().any(|v| *v == Verdict::Approve as i32) {
                "approved"
            } else if f.reviewers.is_empty() {
                "none"
            } else {
                "commented"
            };
        }
        if needs.commenters {
            let mut tx = self.db.begin(super::SURFACE, cx).await?;
            for (c, _) in tx
                .scan_msgs::<super::model::CommentRecord>(
                    &super::COMMENTS,
                    &crate::store::Scan::scope(cr.id.clone(), 10_000),
                )
                .await?
            {
                let a = c
                    .conversation
                    .and_then(|x| x.author)
                    .or(c.review.and_then(|x| x.author));
                if let Some(a) = a {
                    f.commenters.insert(a.id);
                }
            }
        }
        if needs.status
            && let Some(h) = head_of(cr)
        {
            let results = self.checks.check_results(cx, &repo.id, &hex_of(h)).await?;
            use bee_policy::CheckStatus as S;
            f.status = if results.iter().any(|r| r.status == S::Failure) {
                "failure"
            } else if results.is_empty() || results.iter().any(|r| r.status == S::Pending) {
                "pending"
            } else {
                "success"
            };
        }
        Ok(f)
    }

    /// Lists CRs matching `f`, newest first unless the query sorts; paged by offset.
    pub async fn list(
        &self,
        cx: &Cx,
        repo: &Repo,
        f: &ListFilter,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<ChangeRequest>, String)> {
        let compiled = if f.query.trim().is_empty() {
            None
        } else {
            Some(compile(&f.query, &cx.actor_id, self.db.now_ms())?)
        };
        let offset: usize = if token.is_empty() {
            0
        } else {
            token
                .parse()
                .map_err(|_| ForgeError::invalid("bad_page_token", "malformed page token"))?
        };
        let path_prefix = crate::leases::normalize_path(f.path_prefix.as_bytes())?;
        let mut needs = Needs::default();
        let mut ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        if let Some(c) = &compiled {
            walk(&c.filter, &mut needs);
            collect_logins(&c.filter, &mut ids);
            for (login, set) in ids.iter_mut() {
                if let Some(p) = self.directory.by_login(login).await {
                    set.insert(p.id);
                }
            }
        }
        let eval = Eval { repo, ids: &ids };
        let mut hits: Vec<(CrRecord, u64)> = Vec::new();
        for (rec, v) in self.records(cx, &repo.id).await? {
            let cr = rec.cr();
            if f.state != ChangeState::Unspecified && cr.state != f.state as i32 {
                continue;
            }
            if !f.author_id.is_empty() && cr.author.as_ref().is_none_or(|a| a.id != f.author_id) {
                continue;
            }
            if !f.target_ref.is_empty() && cr.target_ref != f.target_ref {
                continue;
            }
            if !f
                .labels
                .iter()
                .all(|l| cr.labels.iter().any(|x| x.name.eq_ignore_ascii_case(l)))
            {
                continue;
            }
            if !f.review_requested_id.is_empty()
                && !cr.review_requests.iter().any(|r| {
                    r.reviewer
                        .as_ref()
                        .is_some_and(|p| p.id == f.review_requested_id)
                        || r.team_id == f.review_requested_id
                })
            {
                continue;
            }
            if !f.assignee_id.is_empty() && !cr.assignees.iter().any(|a| a.id == f.assignee_id) {
                continue;
            }
            if f.draft.is_some_and(|d| d != cr.draft) {
                continue;
            }
            if !path_prefix.is_empty()
                && !rec
                    .paths
                    .iter()
                    .any(|p| crate::leases::covers(&path_prefix, &p.path))
            {
                continue;
            }
            if let Some(c) = &compiled {
                let facts = self.facts(cx, repo, &rec, &needs).await?;
                if !eval.matches(&c.filter, &rec, &facts) {
                    continue;
                }
            }
            hits.push((rec, v));
        }
        if let Some(c) = &compiled {
            let key = |r: &CrRecord| -> i64 {
                let cr = r.cr();
                match c.sort.field {
                    "created" => ms(&cr.created_at) as i64,
                    "updated" => ms(&cr.updated_at) as i64,
                    "comments" => i64::from(r.comment_count),
                    "reactions" => r.reactions.len() as i64,
                    "interactions" => i64::from(r.comment_count) + r.reactions.len() as i64,
                    _ => cr.number as i64,
                }
            };
            hits.sort_by(|a, b| {
                let (ka, kb) = (key(&a.0), key(&b.0));
                let ord = ka.cmp(&kb).then(a.0.cr().number.cmp(&b.0.cr().number));
                if c.sort.order == SortOrder::Asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
        }
        let total = hits.len();
        let page: Vec<ChangeRequest> = hits
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(r, v)| Self::served(&r, v, &cx.actor_id))
            .collect();
        let next = if offset + page.len() < total {
            (offset + page.len()).to_string()
        } else {
            String::new()
        };
        Ok((page, next))
    }
}

fn collect_logins(f: &Filter, out: &mut BTreeMap<String, BTreeSet<String>>) {
    match f {
        Filter::Pred(Predicate::Person { login, .. }) => {
            out.entry(login.clone())
                .or_insert_with(|| BTreeSet::from([login.clone()]));
        }
        Filter::Pred(Predicate::AgentAuthor {
            login: Some(login), ..
        }) => {
            out.entry(login.clone())
                .or_insert_with(|| BTreeSet::from([login.clone()]));
        }
        Filter::Not(x) => collect_logins(x, out),
        Filter::And(v) | Filter::Or(v) => v.iter().for_each(|x| collect_logins(x, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_compile() {
        let c = compile(
            "is:open review:required label:bug author:@me sort:updated-asc",
            "me",
            0,
        )
        .unwrap();
        assert_eq!(c.sort.field, "updated");
        assert_eq!(c.sort.order, SortOrder::Asc);
        let mut n = Needs::default();
        walk(&c.filter, &mut n);
        assert!(n.reviews && !n.status);
        assert!(compile("nonsense:qualifier", "me", 0).is_err());
    }

    #[test]
    fn bounds() {
        assert!(in_bounds(5, &Bound::Included(5), &Bound::Unbounded));
        assert!(!in_bounds(5, &Bound::Excluded(5), &Bound::Unbounded));
        assert!(in_bounds(5, &Bound::Unbounded, &Bound::Excluded(6)));
    }
}
