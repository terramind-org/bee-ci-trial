//! Search: issues and pull requests (GitHub qualifiers, handed to the forge's query
//! language), repositories, code (through the [`crate::ports::CodeSearch`] port), users,
//! commits, labels, topics.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_proto::v1::change_service_server::ChangeService as _;
use bee_proto::v1::issue_service_server::IssueService as _;
use bee_proto::v1::{ChangeRequest, Issue, ListChangesRequest, ListIssuesRequest, Repo};
use serde_json::{Value, json};

use super::{repo_named, rid};
use crate::error::{ApiError, ApiResult};
use crate::http::{Ctx, Params, Reply, all_pages};
use crate::shapes;

/// A parsed search query: qualifiers by key and the free-text terms.
pub(crate) struct Query {
    pub quals: Vec<(String, String)>,
    pub terms: Vec<String>,
}

impl Query {
    /// Parses `key:value` qualifiers (values may be quoted) and free terms.
    pub(crate) fn parse(q: &str) -> Query {
        let mut quals = Vec::new();
        let mut terms = Vec::new();
        let mut cur = String::new();
        let mut tokens = Vec::new();
        let mut quoted = false;
        for c in q.chars() {
            match c {
                '"' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            tokens.push(cur);
        }
        for t in tokens {
            match t.split_once(':') {
                Some((k, v)) if !k.is_empty() && !v.is_empty() && !k.contains('/') => {
                    quals.push((k.to_lowercase(), v.to_string()))
                }
                _ => terms.push(t),
            }
        }
        Query { quals, terms }
    }

    /// Values of a qualifier.
    pub(crate) fn all(&self, k: &str) -> Vec<&str> {
        self.quals
            .iter()
            .filter(|(q, _)| q == k)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The first value of a qualifier.
    pub(crate) fn one(&self, k: &str) -> Option<&str> {
        self.all(k).into_iter().next()
    }

    /// Whether `is:<v>` or `type:<v>` is present.
    pub(crate) fn is(&self, v: &str) -> bool {
        self.all("is").contains(&v) || self.all("type").contains(&v)
    }
}

/// The repos a query targets: `repo:` qualifiers, else `org:`/`user:`, else every repo
/// the caller can see.
async fn target_repos(cx: &Ctx, q: &Query) -> ApiResult<Vec<Repo>> {
    let repos = q.all("repo");
    if !repos.is_empty() {
        let mut out = Vec::new();
        for r in repos {
            let (o, n) = r.split_once('/').ok_or_else(|| {
                ApiError::validation(
                    "Search",
                    "q",
                    "invalid",
                    &format!("repo:{r} is not owner/name"),
                )
            })?;
            match repo_named(cx, o, n).await {
                Ok(repo) => out.push(repo),
                Err(e) if e.status == 404 => {
                    return Err(ApiError::validation(
                        "Search",
                        "q",
                        "invalid",
                        "The listed users and repositories cannot be searched either because the resources do not exist or you do not have permission to view them.",
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        return Ok(out);
    }
    let orgs: Vec<&str> = q.all("org").into_iter().chain(q.all("user")).collect();
    if !orgs.is_empty() {
        let mut out = Vec::new();
        for o in orgs {
            out.extend(super::repos::list_org(cx, o).await.unwrap_or_default());
        }
        return Ok(out);
    }
    super::repos::visible_repos(cx).await
}

/// The query text for the forge: qualifiers other than repo/org/user/type, with `@me`
/// replaced by the caller's login.
async fn forge_query(cx: &Ctx, q: &Query) -> String {
    let me = match &cx.caller {
        Some(c) => cx.login(&cx.principal(&c.cx.actor_id).await).await,
        None => String::new(),
    };
    let mut parts = Vec::new();
    for (k, v) in &q.quals {
        if matches!(k.as_str(), "repo" | "org" | "user" | "type")
            || (k == "is" && matches!(v.as_str(), "pr" | "issue"))
        {
            continue;
        }
        let v = if v == "@me" { me.clone() } else { v.clone() };
        if v.contains(' ') {
            parts.push(format!("{k}:\"{v}\""));
        } else {
            parts.push(format!("{k}:{v}"));
        }
    }
    parts.extend(q.terms.iter().cloned());
    parts.join(" ")
}

/// An issue search hit.
pub(crate) enum Hit {
    Issue(Repo, Box<Issue>),
    Pull(Repo, Box<ChangeRequest>),
}

/// `GET /search/issues`.
pub(crate) async fn issues(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let raw = cx
        .q("q")
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| ApiError::missing_field("Search", "q"))?
        .to_string();
    let hits = issue_hits(&cx, &raw, cx.q("sort"), cx.q("order") == Some("asc")).await?;
    let total = hits.len();
    let (page, per) = cx.page();
    let mut items = Vec::new();
    for h in hits.iter().skip((page - 1) * per).take(per) {
        let mut v = match h {
            Hit::Issue(r, i) => shapes::issue(&cx, r, i).await,
            Hit::Pull(r, c) => shapes::issue_of_pull(&cx, r, c).await,
        };
        v["score"] = json!(1.0);
        items.push(v);
    }
    Ok(wrapped(&cx, items, total))
}

/// Issues and PRs matching a GitHub search query, sorted by `sort` (`created`, `updated`,
/// `comments`), newest first unless `asc`.
pub(crate) async fn issue_hits(
    cx: &Ctx,
    raw: &str,
    sort: Option<&str>,
    asc: bool,
) -> ApiResult<Vec<Hit>> {
    let q = Query::parse(raw);
    let want_pr = q.is("pr");
    let want_issue = q.is("issue");
    let text = forge_query(cx, &q).await;
    let mut hits: Vec<(i64, u64, Hit)> = Vec::new();
    for repo in target_repos(cx, &q).await? {
        if !want_pr {
            let svc = cx.hub.svc.clone();
            let list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListIssuesRequest {
                    repo: rid(&repo.id),
                    all_states: true,
                    query: text.clone(),
                    page: Some(page),
                    ..Default::default()
                });
                async move {
                    let r = svc.list_issues(req).await?.into_inner();
                    Ok((r.issues, r.page))
                }
            })
            .await
            .unwrap_or_default();
            for i in list {
                let t = sort_time(
                    sort,
                    i.created_at.as_ref(),
                    i.updated_at.as_ref(),
                    i.comment_count,
                );
                hits.push((t, i.number, Hit::Issue(repo.clone(), Box::new(i))));
            }
        }
        if !want_issue && let Some(svc) = cx.hub.changes.clone() {
            let list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListChangesRequest {
                    repo: rid(&repo.id),
                    query: text.clone(),
                    page: Some(page),
                    ..Default::default()
                });
                async move {
                    let r = svc.list_changes(req).await?.into_inner();
                    Ok((r.changes, r.page))
                }
            })
            .await
            .unwrap_or_default();
            for c in list {
                let t = sort_time(
                    sort,
                    c.created_at.as_ref(),
                    c.updated_at.as_ref(),
                    c.comment_count,
                );
                hits.push((t, c.number, Hit::Pull(repo.clone(), Box::new(c))));
            }
        }
    }
    hits.sort_by(|a, b| {
        let o = (a.0, a.1).cmp(&(b.0, b.1));
        if asc { o } else { o.reverse() }
    });
    Ok(hits.into_iter().map(|(_, _, h)| h).collect())
}

fn sort_time(
    sort: Option<&str>,
    created: Option<&pbjson_types::Timestamp>,
    updated: Option<&pbjson_types::Timestamp>,
    comments: u32,
) -> i64 {
    match sort {
        Some("updated") => updated.or(created).map(|t| t.seconds).unwrap_or(0),
        Some("comments") => comments as i64,
        _ => created.map(|t| t.seconds).unwrap_or(0),
    }
}

/// Repos matching a GitHub repository search query.
pub(crate) async fn repo_hits(cx: &Ctx, raw: &str) -> ApiResult<Vec<Repo>> {
    let q = Query::parse(raw);
    let terms: Vec<String> = q.terms.iter().map(|t| t.to_lowercase()).collect();
    let mut repos = target_repos(cx, &q).await?;
    repos.retain(|r| {
        let hay =
            format!("{} {} {}", r.full_name, r.description, r.topics.join(" ")).to_lowercase();
        terms.iter().all(|t| hay.contains(t.as_str()))
            && q.all("topic")
                .iter()
                .all(|t| r.topics.iter().any(|x| x.eq_ignore_ascii_case(t)))
            && q.all("language")
                .iter()
                .all(|l| r.primary_language.eq_ignore_ascii_case(l))
            && (!q.is("public") || shapes::visibility(r) == "public")
            && (!q.is("private") || shapes::visibility(r) != "public")
            && match q.one("fork") {
                Some("only") => !r.fork_parent_id.is_empty(),
                Some("true") => true,
                _ => r.fork_parent_id.is_empty(),
            }
            && match q.one("archived") {
                Some("true") => r.archived,
                Some("false") => !r.archived,
                _ => true,
            }
    });
    Ok(repos)
}

/// A search page: `{total_count, incomplete_results, items}` with a `Link` header.
fn wrapped(cx: &Ctx, items: Vec<Value>, total: usize) -> Reply {
    let mut r = cx.paged(items, total);
    if let crate::http::Body::Json(Value::Array(a)) = &r.body {
        r.body = crate::http::Body::Json(json!({
            "total_count": total,
            "incomplete_results": false,
            "items": a,
        }));
    }
    r
}

fn page_of(cx: &Ctx, all: Vec<Value>) -> Reply {
    let total = all.len();
    let (page, per) = cx.page();
    let items = all.into_iter().skip((page - 1) * per).take(per).collect();
    wrapped(cx, items, total)
}

/// `GET /search/repositories`.
pub(crate) async fn repositories(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let raw = cx
        .q("q")
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| ApiError::missing_field("Search", "q"))?;
    let mut repos = repo_hits(&cx, raw).await?;
    match cx.q("sort") {
        Some("stars") => repos.sort_by_key(|r| r.stars),
        Some("forks") | Some("help-wanted-issues") => repos.sort_by_key(|r| r.forks),
        Some("updated") => {
            repos.sort_by_key(|r| r.pushed_at.as_ref().map(|t| t.seconds).unwrap_or(0))
        }
        _ => repos.sort_by(|a, b| b.full_name.cmp(&a.full_name)),
    }
    if cx.q("order") != Some("asc") {
        repos.reverse();
    }
    let all = repos
        .iter()
        .map(|r| {
            let mut v = shapes::repo(&cx, r, None, None);
            v["score"] = json!(1.0);
            v
        })
        .collect();
    Ok(page_of(&cx, all))
}

/// `GET /search/code`.
pub(crate) async fn code(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let raw = cx
        .q("q")
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| ApiError::missing_field("Search", "q"))?;
    let q = Query::parse(raw);
    if q.terms.is_empty() {
        return Err(ApiError::validation(
            "Search",
            "q",
            "invalid",
            "code search needs at least one search term",
        ));
    }
    let repos = target_repos(&cx, &q).await?;
    let hits = cx
        .hub
        .ports
        .code_search
        .search(&repos, &q.terms.join(" "), crate::http::LIST_CAP)
        .await?;
    let paths = q.all("path");
    let exts: Vec<&str> = q.all("extension");
    let by_id: BTreeMap<&str, &Repo> = repos.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut all = Vec::new();
    for h in &hits {
        if !paths
            .iter()
            .all(|p| h.path.starts_with(p.trim_start_matches('/')))
        {
            continue;
        }
        if !exts.iter().all(|e| h.path.ends_with(&format!(".{e}"))) {
            continue;
        }
        let Some(repo) = by_id.get(h.repo_id.as_str()) else {
            continue;
        };
        let name = h.path.rsplit('/').next().unwrap_or(&h.path);
        let mut v = json!({
            "name": name,
            "path": h.path,
            "sha": h.sha,
            "url": cx.api(&format!("/repos/{}/contents/{}", repo.full_name, h.path)),
            "git_url": cx.api(&format!("/repos/{}/git/blobs/{}", repo.full_name, h.sha)),
            "html_url": cx.html(&format!("/{}/blob/{}/{}", repo.full_name, repo.default_ref, h.path)),
            "repository": shapes::repo(&cx, repo, None, None),
            "score": 1.0,
        });
        if cx.accepts("text-match+json") {
            v["text_matches"] = Value::Array(
                h.fragments
                    .iter()
                    .map(|f| json!({"object_type": "FileContent", "property": "content", "fragment": f, "matches": []}))
                    .collect(),
            );
        }
        all.push(v);
    }
    Ok(page_of(&cx, all))
}

/// `GET /search/users`: members of the orgs the caller can see, by login or name.
pub(crate) async fn users(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    use bee_proto::v1::org_service_server::OrgService as _;
    let raw = cx
        .q("q")
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| ApiError::missing_field("Search", "q"))?;
    let q = Query::parse(raw);
    let terms: Vec<String> = q.terms.iter().map(|t| t.to_lowercase()).collect();
    let mut seen = BTreeSet::new();
    let mut all = Vec::new();
    for o in super::repos::orgs_of(&cx, "").await? {
        let svc = cx.hub.svc.clone();
        let members = all_pages(|page| {
            let svc = svc.clone();
            let req = cx.req(bee_proto::v1::ListMembersRequest {
                org: o.name.clone(),
                page: Some(page),
            });
            async move {
                let r = svc.list_members(req).await?.into_inner();
                Ok((r.members, r.page))
            }
        })
        .await
        .unwrap_or_default();
        for m in members {
            let Some(p) = m.principal else { continue };
            if !seen.insert(p.id.clone()) {
                continue;
            }
            let login = cx.login(&p).await;
            let hay = format!("{login} {}", p.display).to_lowercase();
            let kind_ok = match q.one("type") {
                Some("org") => false,
                Some("user") | None => true,
                _ => true,
            };
            if kind_ok && terms.iter().all(|t| hay.contains(t.as_str())) {
                let mut v = shapes::user(&cx, &p).await;
                v["score"] = json!(1.0);
                all.push(v);
            }
        }
    }
    Ok(page_of(&cx, all))
}

/// `GET /search/commits`: messages on the default branch of the target repos.
pub(crate) async fn commits(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let raw = cx
        .q("q")
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| ApiError::missing_field("Search", "q"))?;
    let q = Query::parse(raw);
    let terms: Vec<String> = q.terms.iter().map(|t| t.to_lowercase()).collect();
    let git = &cx.hub.ports.git;
    let mut all = Vec::new();
    for repo in target_repos(&cx, &q).await? {
        let branch = if repo.default_ref.is_empty() {
            "main"
        } else {
            repo.default_ref.as_str()
        };
        let Some(head) = git.resolve(&repo, branch).await? else {
            continue;
        };
        for c in git.log(&repo, &head, None, 0, 1000).await? {
            let msg = c.message.to_lowercase();
            if terms.iter().all(|t| msg.contains(t.as_str()))
                && q.all("author-name")
                    .iter()
                    .all(|a| c.author.name.eq_ignore_ascii_case(a))
                && q.all("author")
                    .iter()
                    .all(|a| c.author.name.eq_ignore_ascii_case(a))
                && q.all("hash").iter().all(|h| c.sha.starts_with(h))
            {
                let mut v = shapes::commit(&cx, &repo, &c).await;
                v["repository"] = shapes::repo(&cx, &repo, None, None);
                v["score"] = json!(1.0);
                all.push(v);
            }
        }
    }
    Ok(page_of(&cx, all))
}

/// `GET /search/labels?repository_id=&q=`.
pub(crate) async fn labels(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let rid_num: u64 = cx
        .q("repository_id")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::missing_field("Search", "repository_id"))?;
    let q = cx.q("q").unwrap_or("").to_lowercase();
    let repo = match cx.hub.ids.get("repo", rid_num) {
        Some(ix) => {
            let (o, n) = ix.context.split_once('/').ok_or_else(ApiError::not_found)?;
            repo_named(&cx, o, n).await?
        }
        None => super::repos::visible_repos(&cx)
            .await?
            .into_iter()
            .find(|r| crate::ids::num("repo", &r.id) == rid_num)
            .ok_or_else(ApiError::not_found)?,
    };
    let svc = cx.hub.svc.clone();
    let list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(bee_proto::v1::ListIssueLabelsRequest {
            repo: rid(&repo.id),
            query: String::new(),
            page: Some(page),
        });
        async move {
            let r = svc.list_issue_labels(req).await?.into_inner();
            Ok((r.labels, r.page))
        }
    })
    .await?;
    let all = list
        .iter()
        .filter(|l| {
            q.split_whitespace().all(|t| {
                l.name.to_lowercase().contains(t) || l.description.to_lowercase().contains(t)
            })
        })
        .map(|l| {
            let mut v = shapes::label(&cx, &repo, l);
            v["score"] = json!(1.0);
            v
        })
        .collect();
    Ok(page_of(&cx, all))
}

/// `GET /search/topics`: topics of the repos the caller can see.
pub(crate) async fn topics(cx: Arc<Ctx>, _: Params) -> ApiResult<Reply> {
    let q = cx.q("q").unwrap_or("").to_lowercase();
    let mut set = BTreeSet::new();
    for r in super::repos::visible_repos(&cx).await? {
        for t in r.topics {
            if q.split_whitespace().all(|w| t.to_lowercase().contains(w)) {
                set.insert(t);
            }
        }
    }
    let all = set
        .into_iter()
        .map(|t| {
            json!({
                "name": t, "display_name": t, "short_description": Value::Null, "description": Value::Null,
                "created_by": Value::Null, "released": Value::Null, "created_at": Value::Null, "updated_at": Value::Null,
                "featured": false, "curated": false, "score": 1.0,
            })
        })
        .collect();
    Ok(page_of(&cx, all))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_split_into_qualifiers_and_terms() {
        let q =
            Query::parse("repo:acme/api is:pr is:open author:@me label:\"needs review\" crash fix");
        assert_eq!(q.all("repo"), vec!["acme/api"]);
        assert!(q.is("pr"));
        assert_eq!(q.one("label"), Some("needs review"));
        assert_eq!(q.terms, vec!["crash", "fix"]);
    }
}
