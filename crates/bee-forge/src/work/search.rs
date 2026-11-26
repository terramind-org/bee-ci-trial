//! Issue list filters: a compiled forge query (`bee-search`) evaluated over the issue tables.
//! Lists never depend on a search index (PRODUCT §15.1); the evaluator reads the repo's issue
//! rows in chunks and the few side tables a query needs.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use bee_proto::v1::{Comment, Issue, IssueState, IssueStateReason, Label, Milestone};
use bee_search::forge::{Filter, Key as Q, Predicate, SortOrder, SortSpec};

use super::issues::{Cache, IssuePage, render};
use super::model::IssueRec;
use super::*;

/// Which states a list returns when the query names none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StateSelector {
    /// Open issues.
    #[default]
    Open,
    /// Closed issues.
    Closed,
    /// Both.
    All,
}

/// Sort fields of a list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListSort {
    /// Creation time.
    #[default]
    Created,
    /// Last update.
    Updated,
    /// Comment count.
    Comments,
    /// Reaction count.
    Reactions,
}

/// An issue list request, with every principal and name the service resolved.
#[derive(Clone, Debug, Default)]
pub struct IssueListQuery {
    /// The compiled `query` string (`Filter::All` when none).
    pub filter: Option<Filter>,
    /// The query's `sort:` when it gave one.
    pub query_sort: Option<SortSpec>,
    /// State selection used when the filter has no state predicate.
    pub state: StateSelector,
    /// Labels that must all be present.
    pub labels: Vec<String>,
    /// `assignee`: id, `none` or `*`.
    pub assignee: String,
    /// `author` id.
    pub author: String,
    /// `mentioned` id.
    pub mentioned: String,
    /// Milestone number or title, `none` or `*`.
    pub milestone: String,
    /// Issue type name, `none` or `*`.
    pub issue_type: String,
    /// Updated at or after (ms).
    pub since_ms: Option<u64>,
    /// Request-level sort.
    pub sort: ListSort,
    /// Ascending order for the request-level sort.
    pub ascending: bool,
    /// Lower-cased login or handle to principal id, for the `Person` predicates.
    pub people: BTreeMap<String, String>,
    /// `public`, `private` or `internal`.
    pub visibility: String,
    /// The repo is archived.
    pub archived: bool,
}

fn has_state(f: &Filter) -> bool {
    match f {
        Filter::Pred(Predicate::Enum { field, .. }) => *field == "state",
        Filter::Not(i) => has_state(i),
        Filter::And(v) | Filter::Or(v) => v.iter().any(has_state),
        _ => false,
    }
}

fn walk(f: &Filter, visit: &mut dyn FnMut(&Predicate)) {
    match f {
        Filter::Pred(p) => visit(p),
        Filter::Not(i) => walk(i, visit),
        Filter::And(v) | Filter::Or(v) => v.iter().for_each(|c| walk(c, visit)),
        Filter::All => {}
    }
}

/// What the evaluator needs besides the row.
struct Ctx<'a> {
    repo: &'a RepoInfo,
    q: &'a IssueListQuery,
    labels: BTreeMap<String, Label>,
    milestones: BTreeMap<u64, Milestone>,
    types: BTreeMap<String, String>,
    comments: BTreeMap<String, String>,
    projects: BTreeMap<String, BTreeSet<String>>,
    in_any_project: BTreeSet<String>,
}

fn within(v: i64, min: &Bound<i64>, max: &Bound<i64>) -> bool {
    (match min {
        Bound::Included(m) => v >= *m,
        Bound::Excluded(m) => v > *m,
        Bound::Unbounded => true,
    }) && (match max {
        Bound::Included(m) => v <= *m,
        Bound::Excluded(m) => v < *m,
        Bound::Unbounded => true,
    })
}

impl Ctx<'_> {
    fn person_id(&self, login: &str) -> String {
        self.q
            .people
            .get(&login.to_lowercase())
            .cloned()
            .unwrap_or_else(|| login.to_string())
    }

    fn is_person(&self, p: &Option<bee_proto::v1::Principal>, login: &str) -> bool {
        let id = self.person_id(login);
        p.as_ref()
            .is_some_and(|p| p.id == id || p.display.eq_ignore_ascii_case(login))
    }

    fn eval(&self, f: &Filter, r: &IssueRec) -> bool {
        match f {
            Filter::All => true,
            Filter::Pred(p) => self.pred(p, r),
            Filter::Not(i) => !self.eval(i, r),
            Filter::And(v) => v.iter().all(|c| self.eval(c, r)),
            Filter::Or(v) => v.iter().any(|c| self.eval(c, r)),
        }
    }

    fn label_names(&self, r: &IssueRec) -> Vec<String> {
        r.label_ids
            .iter()
            .filter_map(|id| self.labels.get(id).map(|l| l.name.to_lowercase()))
            .collect()
    }

    fn pred(&self, p: &Predicate, r: &IssueRec) -> bool {
        match p {
            Predicate::Text { text, fields } => {
                let needle = text.to_lowercase();
                fields.iter().any(|f| match *f {
                    "title" => r.title.to_lowercase().contains(&needle),
                    "body" => r.body.to_lowercase().contains(&needle),
                    "comments" => self
                        .comments
                        .get(&r.id)
                        .is_some_and(|c| c.contains(&needle)),
                    _ => false,
                })
            }
            Predicate::Person { role, login } => {
                let id = self.person_id(login);
                match role {
                    Q::Author => self.is_person(&r.author, login),
                    Q::Assignee => r
                        .assignees
                        .iter()
                        .any(|a| a.id == id || a.display.eq_ignore_ascii_case(login)),
                    Q::Commenter => r.commenter_ids.contains(&id),
                    Q::Mentions => r.mentioned_ids.contains(&id),
                    Q::Involves => {
                        self.is_person(&r.author, login)
                            || r.assignees.iter().any(|a| a.id == id)
                            || r.commenter_ids.contains(&id)
                            || r.mentioned_ids.contains(&id)
                    }
                    _ => false,
                }
            }
            Predicate::AgentAuthor { is_agent, login } => {
                let agent = r
                    .author
                    .as_ref()
                    .is_some_and(|a| a.kind == bee_proto::v1::PrincipalKind::Agent as i32);
                if agent != *is_agent {
                    return false;
                }
                match login {
                    Some(l) => self.is_person(&r.author, l),
                    None => true,
                }
            }
            Predicate::Name { field, value } => {
                let v = value.to_lowercase();
                match field {
                    Q::Label => self.label_names(r).contains(&v) || r.label_ids.contains(value),
                    Q::Milestone => {
                        r.milestone_number > 0
                            && self.milestones.get(&r.milestone_number).is_some_and(|m| {
                                m.title.to_lowercase() == v || value.parse() == Ok(m.number)
                            })
                    }
                    Q::Project => self.projects.get(&v).is_some_and(|s| s.contains(&r.id)),
                    Q::Repo => self.repo.full_name.to_lowercase() == v,
                    Q::Org | Q::User => {
                        self.repo
                            .full_name
                            .split('/')
                            .next()
                            .unwrap_or("")
                            .to_lowercase()
                            == v
                    }
                    _ => false,
                }
            }
            Predicate::PathPrefix(_) => false,
            Predicate::Enum { field, value } => match *field {
                "state" => match *value {
                    "open" => r.state != IssueState::Closed as i32,
                    "closed" => r.state == IssueState::Closed as i32,
                    _ => false,
                },
                "type" => *value == "issue",
                "visibility" => self.q.visibility == *value,
                "reason" => {
                    r.state == IssueState::Closed as i32
                        && match *value {
                            "completed" => r.state_reason == IssueStateReason::Completed as i32,
                            "not-planned" => r.state_reason == IssueStateReason::NotPlanned as i32,
                            "duplicate" => r.state_reason == IssueStateReason::Duplicate as i32,
                            _ => false,
                        }
                }
                "linked" => match *value {
                    "pr" | "cr" => !r.linked_change_ids.is_empty(),
                    "issue" => {
                        !r.parent_id.is_empty()
                            || r.child_count > 0
                            || r.blocked_by_count > 0
                            || r.blocking_count > 0
                            || r.duplicate_of.is_some()
                    }
                    _ => false,
                },
                _ => false,
            },
            Predicate::Flag { field, value } => {
                let actual = match *field {
                    "locked" => r.locked,
                    "archived" => self.q.archived,
                    "pinned" => r.pinned,
                    _ => false,
                };
                actual == *value
            }
            Predicate::Range { field, min, max } => {
                let v = match field {
                    Q::Created => r.created_ms as i64,
                    Q::Updated => r.updated_ms as i64,
                    Q::Closed => {
                        if r.closed_ms == 0 {
                            return false;
                        }
                        r.closed_ms as i64
                    }
                    Q::Comments => i64::from(r.comment_count),
                    Q::Reactions => i64::from(r.reaction_total),
                    Q::Interactions => i64::from(r.comment_count + r.reaction_total),
                    _ => return false,
                };
                within(v, min, max)
            }
            Predicate::Missing(w) => match *w {
                "label" => r.label_ids.is_empty(),
                "milestone" => r.milestone_number == 0,
                "assignee" => r.assignees.is_empty(),
                "project" => !self.in_any_project.contains(&r.id),
                "reviewer" => true,
                _ => false,
            },
        }
    }

    /// The request-level fields (`labels`, `assignee`, ...) as conditions.
    fn request_matches(&self, r: &IssueRec) -> bool {
        let q = self.q;
        if !q.labels.is_empty() {
            let have = self.label_names(r);
            if !q.labels.iter().all(|l| have.contains(&l.to_lowercase())) {
                return false;
            }
        }
        match q.assignee.as_str() {
            "" => {}
            "none" => {
                if !r.assignees.is_empty() {
                    return false;
                }
            }
            "*" => {
                if r.assignees.is_empty() {
                    return false;
                }
            }
            id => {
                if !r.assignees.iter().any(|a| a.id == id) {
                    return false;
                }
            }
        }
        if !q.author.is_empty()
            && r.author.as_ref().map(|a| a.id.as_str()) != Some(q.author.as_str())
        {
            return false;
        }
        if !q.mentioned.is_empty() && !r.mentioned_ids.contains(&q.mentioned) {
            return false;
        }
        match q.milestone.as_str() {
            "" => {}
            "none" => {
                if r.milestone_number != 0 {
                    return false;
                }
            }
            "*" => {
                if r.milestone_number == 0 {
                    return false;
                }
            }
            m => {
                let ok = r.milestone_number > 0
                    && self.milestones.get(&r.milestone_number).is_some_and(|ms| {
                        ms.title.eq_ignore_ascii_case(m) || m.parse() == Ok(ms.number)
                    });
                if !ok {
                    return false;
                }
            }
        }
        match q.issue_type.as_str() {
            "" => {}
            "none" => {
                if !r.issue_type_id.is_empty() {
                    return false;
                }
            }
            "*" => {
                if r.issue_type_id.is_empty() {
                    return false;
                }
            }
            t => {
                let ok = self
                    .types
                    .get(&r.issue_type_id)
                    .is_some_and(|n| n.eq_ignore_ascii_case(t));
                if !ok {
                    return false;
                }
            }
        }
        if let Some(s) = q.since_ms
            && r.updated_ms < s
        {
            return false;
        }
        true
    }
}

fn sort_key(field: &str, r: &IssueRec) -> i64 {
    match field {
        "updated" => r.updated_ms as i64,
        "comments" => i64::from(r.comment_count),
        "reactions" => i64::from(r.reaction_total),
        "interactions" => i64::from(r.comment_count + r.reaction_total),
        _ => r.created_ms as i64,
    }
}

impl Work {
    /// Every issue of a repo as `(number, opened ms, closed ms or 0)`, by number: the
    /// insights activity the server's `ActivitySource` reports.
    pub async fn issues_in_window(&self, cx: &Cx, repo_id: &str) -> Result<Vec<(u64, u64, u64)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all = scan_all_msgs::<IssueRec>(&mut tx, &ISSUES, Scan::scope(repo_id.to_string(), 1))
            .await?;
        tx.commit().await?;
        let mut out: Vec<(u64, u64, u64)> = all
            .into_iter()
            .map(|(r, _)| (r.number, r.created_ms, r.closed_ms))
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    /// Lists a repo's issues: state selection, request fields and the compiled query, sorted
    /// and paged (the page token is the sort key of the last issue returned).
    pub async fn list_issues(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        q: &IssueListQuery,
        limit: usize,
        token: &str,
    ) -> Result<IssuePage> {
        let filter = q.filter.clone().unwrap_or(Filter::All);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(repo.id.clone(), 1);
        if let Some(s) = q.since_ms {
            scan = scan.n1_min(s as i64);
        }
        let all = scan_all_msgs::<IssueRec>(&mut tx, &ISSUES, scan).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let labels = cache.labels(&mut tx, &repo.id).await?.clone();
        let milestones = cache.milestones(&mut tx, &repo.id).await?.clone();
        let types: BTreeMap<String, String> = cache
            .types(&mut tx, &repo.org_id)
            .await?
            .iter()
            .map(|(k, t)| (k.clone(), t.name.clone()))
            .collect();
        // Side tables, only when the query reads them.
        let (mut need_comments, mut need_projects) = (false, false);
        walk(&filter, &mut |p| match p {
            Predicate::Text { fields, .. } if fields.contains(&"comments") => need_comments = true,
            Predicate::Name {
                field: Q::Project, ..
            }
            | Predicate::Missing("project") => need_projects = true,
            _ => {}
        });
        let mut comments = BTreeMap::new();
        if need_comments {
            for (rec, _) in &all {
                let mut text = String::new();
                for (c, _) in
                    scan_all_msgs::<Comment>(&mut tx, &COMMENTS, Scan::scope(rec.id.clone(), 1))
                        .await?
                {
                    text.push_str(&c.body.to_lowercase());
                    text.push('\n');
                }
                comments.insert(rec.id.clone(), text);
            }
        }
        let (mut projects, mut in_any_project) = (BTreeMap::new(), BTreeSet::new());
        if need_projects {
            let (p, a) = super::projects::issue_project_sets(&mut tx, &repo.org_id).await?;
            projects = p;
            in_any_project = a;
        }
        let ctx = Ctx {
            repo,
            q,
            labels,
            milestones,
            types,
            comments,
            projects,
            in_any_project,
        };
        let state_in_query = has_state(&filter);
        let (field, descending) = match q.query_sort {
            Some(s) if s.field != "best-match" => (s.field, s.order == SortOrder::Desc),
            _ => (
                match q.sort {
                    ListSort::Created => "created",
                    ListSort::Updated => "updated",
                    ListSort::Comments => "comments",
                    ListSort::Reactions => "reactions",
                },
                !q.ascending,
            ),
        };
        let mut hits: Vec<((i64, u64), IssueRec, u64)> = Vec::new();
        for (rec, row) in all {
            if !state_in_query {
                let closed = rec.state == IssueState::Closed as i32;
                let ok = match q.state {
                    StateSelector::Open => !closed,
                    StateSelector::Closed => closed,
                    StateSelector::All => true,
                };
                if !ok {
                    continue;
                }
            }
            if !ctx.request_matches(&rec) || !ctx.eval(&filter, &rec) {
                continue;
            }
            hits.push(((sort_key(field, &rec), rec.number), rec, row.version));
        }
        hits.sort_by_key(|h| h.0);
        if descending {
            hits.reverse();
        }
        let total = hits.len() as u64;
        let after: Option<(i64, u64)> = token
            .split_once(':')
            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
        let start = match after {
            None => 0,
            Some(t) => hits
                .iter()
                .position(|h| if descending { h.0 < t } else { h.0 > t })
                .unwrap_or(hits.len()),
        };
        let rest = &hits[start..];
        let page: Vec<&((i64, u64), IssueRec, u64)> = rest.iter().take(limit).collect();
        let next = if rest.len() > limit {
            let k = page.last().map(|h| h.0).unwrap_or((0, 0));
            format!("{}:{}", k.0, k.1)
        } else {
            String::new()
        };
        let mut issues: Vec<Issue> = Vec::with_capacity(page.len());
        for h in page {
            issues.push(render(&mut tx, &mut cache, &h.1, h.2).await?);
        }
        Ok(IssuePage {
            issues,
            total,
            next,
        })
    }
}
