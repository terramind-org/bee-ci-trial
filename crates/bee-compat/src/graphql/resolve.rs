//! Field resolvers: GraphQL objects over the same forge calls and helpers as the REST
//! endpoints, so both views agree (numeric `databaseId`s equal REST `id`s).

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bee_proto::v1::change_service_server::ChangeService as _;
use bee_proto::v1::check_service_server::CheckService as _;
use bee_proto::v1::issue_service_server::IssueService as _;
use bee_proto::v1::org_service_server::OrgService as _;
use bee_proto::v1::release_service_server::ReleaseService as _;
use bee_proto::v1::repo_service_server::RepoService as _;
use bee_proto::v1::team_service_server::TeamService as _;
use bee_proto::v1::user_service_server::UserService as _;
use bee_proto::v1::workflow_service_server::WorkflowService as _;
use bee_proto::v1::{
    ChangeRequest, CheckRun, CheckStatus, Comment, CommitStatusState, DisableAutoMergeRequest,
    EnableAutoMergeRequest, EvaluateLandingRequest, GetLatestReleaseRequest, GetOrgRequest,
    GetRepoRequest, GetTeamRequest, GetWorkflowJobRequest, GetWorkflowRunRequest, Issue,
    IssueState, IssueStateReason, Label, LandMethod, LandingEvaluation, ListChecksRequest,
    ListCollaboratorsRequest, ListIssueAssignableUsersRequest, ListIssueLabelsRequest,
    ListIssueMilestonesRequest, ListReleasesRequest, ListReviewsRequest, ListTeamsRequest,
    Milestone, MilestoneState, Org, Principal, ReactionContent, ReactionGroup, Release, Repo,
    RepoRole, Review, StarRepoRequest, UnstarRepoRequest, Verdict, Visibility,
};
use serde_json::{Map, Value, json};

use super::exec::{Exec, GResult, GqlError, Node, Out, json_out};
use super::schema::schema;
use crate::error::ApiError;
use crate::http::{Ctx, all_pages};
use crate::ids::{node, num, parse_node};
use crate::ports::{GitCommitInfo, PathEntry};
use crate::rest::issues::Subject;
use crate::rest::{pulls, rid};
use crate::shapes;
use crate::time::{iso, iso_or, iso_secs};

type Args = Map<String, Value>;

fn a_str<'a>(args: &'a Args, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str)
}

fn a_u64(args: &Args, k: &str) -> Option<u64> {
    args.get(k).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn a_bool(args: &Args, k: &str) -> Option<bool> {
    args.get(k).and_then(Value::as_bool)
}

fn a_list(args: &Args, k: &str) -> Vec<String> {
    match args.get(k) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn opt_ts(t: &Option<pbjson_types::Timestamp>) -> Out {
    Out::Leaf(iso(t))
}

fn cursor(i: usize) -> String {
    STANDARD.encode(format!("cursor:{i}"))
}

fn uncursor(c: &str) -> Option<usize> {
    let raw = STANDARD.decode(c).ok()?;
    String::from_utf8(raw)
        .ok()?
        .strip_prefix("cursor:")?
        .parse()
        .ok()
}

/// A connection over all `items`, cut by `first`/`after`/`last`/`before`.
fn connection(ty: &str, items: Vec<Out>, args: &Args, extra: Vec<(&str, Out)>) -> Out {
    let total = items.len();
    let mut start = a_str(args, "after")
        .and_then(uncursor)
        .map(|i| i + 1)
        .unwrap_or(0)
        .min(total);
    let mut end = a_str(args, "before")
        .and_then(uncursor)
        .unwrap_or(total)
        .min(total)
        .max(start);
    if let Some(f) = a_u64(args, "first") {
        end = end.min(start + f as usize);
    }
    if let Some(l) = a_u64(args, "last") {
        start = start.max(end.saturating_sub(l as usize));
    }
    let edge_ty = format!("{}Edge", ty.trim_end_matches("Connection"));
    let window: Vec<(usize, Out)> = items
        .into_iter()
        .enumerate()
        .skip(start)
        .take(end - start)
        .collect();
    let edges: Vec<Out> = window
        .iter()
        .map(|(i, n)| {
            Out::map(
                &edge_ty,
                vec![("cursor", Out::leaf(cursor(*i))), ("node", n.clone())],
            )
        })
        .collect();
    let nodes: Vec<Out> = window.into_iter().map(|(_, n)| n).collect();
    let mut fields = vec![
        ("nodes", Out::List(nodes)),
        ("edges", Out::List(edges)),
        ("totalCount", Out::leaf(total)),
        (
            "pageInfo",
            Out::map(
                "PageInfo",
                vec![
                    ("hasNextPage", Out::leaf(end < total)),
                    ("hasPreviousPage", Out::leaf(start > 0)),
                    (
                        "startCursor",
                        if end > start {
                            Out::leaf(cursor(start))
                        } else {
                            Out::Null
                        },
                    ),
                    (
                        "endCursor",
                        if end > start {
                            Out::leaf(cursor(end - 1))
                        } else {
                            Out::Null
                        },
                    ),
                ],
            ),
        ),
    ];
    fields.extend(extra);
    Out::map(ty, fields)
}

fn empty_connection(ty: &str, args: &Args) -> Out {
    connection(ty, Vec::new(), args, Vec::new())
}

fn actor(p: &Principal) -> Out {
    if p.id.is_empty() {
        Out::Null
    } else {
        Out::Obj(Node::Actor(Arc::new(p.clone())))
    }
}

fn actor_opt(p: &Option<Principal>) -> Out {
    p.as_ref().map(actor).unwrap_or(Out::Null)
}

fn ghost() -> Out {
    Out::map(
        "User",
        vec![
            ("login", Out::leaf("ghost")),
            ("id", Out::leaf(node("User", "ghost"))),
            ("name", Out::Null),
        ],
    )
}

/// The repo of a numbered node key `repo_id:n`.
async fn repo_by_id(cx: &Ctx, id: &str) -> Result<Repo, GqlError> {
    Ok(cx
        .hub
        .svc
        .get_repo(cx.req(GetRepoRequest { repo: rid(id) }))
        .await?
        .into_inner())
}

/// Whether a PR's head comes from a fork.
fn is_cross(repo: &Repo, c: &ChangeRequest) -> bool {
    !c.head_repo_id.is_empty() && c.head_repo_id != repo.id
}

/// The repo a PR's head lives in: its fork, or the repo itself.
async fn pull_head_repo(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> Repo {
    if is_cross(repo, c)
        && let Ok(f) = repo_by_id(cx, &c.head_repo_id).await
    {
        return f;
    }
    repo.clone()
}

async fn numbered(cx: &Ctx, key: &str) -> Result<(Repo, u64), GqlError> {
    let (rid_, n) = key
        .rsplit_once(':')
        .ok_or_else(|| GqlError::new("NOT_FOUND", "bad node id"))?;
    let n: u64 = n
        .parse()
        .map_err(|_| GqlError::new("NOT_FOUND", "bad node id"))?;
    Ok((repo_by_id(cx, rid_).await?, n))
}

fn not_found(what: &str) -> GqlError {
    GqlError::new(
        "NOT_FOUND",
        format!("Could not resolve to a node with the global id of '{what}'."),
    )
}

/// The issue or PR a node id names.
async fn subject_of(cx: &Ctx, id: &str) -> Result<(Repo, Subject), GqlError> {
    let n = parse_node(id).ok_or_else(|| not_found(id))?;
    if n.kind != "Issue" && n.kind != "PullRequest" {
        return Err(not_found(id));
    }
    let (repo, num_) = numbered(cx, &n.key).await?;
    let sub = crate::rest::issues::subject(cx, &repo, num_).await?;
    Ok((repo, sub))
}

async fn pull_of(cx: &Ctx, id: &str) -> Result<(Repo, ChangeRequest), GqlError> {
    match subject_of(cx, id).await? {
        (repo, Subject::Pull(c)) => Ok((repo, *c)),
        _ => Err(not_found(id)),
    }
}

async fn issue_of(cx: &Ctx, id: &str) -> Result<(Repo, Issue), GqlError> {
    match subject_of(cx, id).await? {
        (repo, Subject::Issue(i)) => Ok((repo, *i)),
        _ => Err(not_found(id)),
    }
}

fn subject_out(repo: &Repo, sub: Subject) -> Out {
    let r = Arc::new(repo.clone());
    match sub {
        Subject::Issue(i) => Out::Obj(Node::Issue(r, Arc::new(*i))),
        Subject::Pull(c) => Out::Obj(Node::Pull(r, Arc::new(*c))),
    }
}

/// Resolves `name` on `node`.
pub(crate) async fn field(ex: &Exec<'_>, n: &Node, name: &str, args: &Args) -> GResult {
    match n {
        Node::Query => query(ex, name, args).await,
        Node::Mutation => mutation(ex, name, args).await,
        Node::Map(_, m) => Ok(m.get(name).cloned().unwrap_or(Out::Null)),
        Node::Json(ty, v) => Ok(match v.get(name) {
            Some(x) => ex.json_child(ty, name, x),
            None => Out::Null,
        }),
        Node::Repo(r) => repo_field(ex, r, name, args).await,
        Node::Pull(r, c) => pull_field(ex, r, c, name, args).await,
        Node::Issue(r, i) => issue_field(ex, r, i, name, args).await,
        Node::Actor(p) => actor_field(ex, p, name, args).await,
        Node::Org(o) => org_field(ex, o, name, args).await,
        Node::Commit(r, c, target) => commit_field(ex, r, c, target.as_deref(), name, args).await,
        Node::Rollup(r, sha, target) => {
            rollup_field(ex, r, sha, target.as_deref(), name, args).await
        }
        Node::Release(r, rel) => release_field(ex, r, rel, name, args).await,
    }
}

// ----- root ---------------------------------------------------------------------------------

async fn query(ex: &Exec<'_>, name: &str, args: &Args) -> GResult {
    let cx = &ex.cx;
    match name {
        "__schema" => Ok(json_out("__Schema", &schema().schema_json())),
        "__type" => Ok(json_out(
            "__Type",
            &schema().type_json(a_str(args, "name").unwrap_or("")),
        )),
        "viewer" => {
            let me = cx.caller().map_err(GqlError::from)?.cx.actor_id.clone();
            Ok(actor(&cx.principal(&me).await))
        }
        "repository" => {
            let owner = a_str(args, "owner").unwrap_or("");
            let repo = a_str(args, "name").unwrap_or("");
            match crate::rest::repo_named(cx, owner, repo).await {
                Ok(r) => Ok(Out::Obj(Node::Repo(Arc::new(r)))),
                Err(e) if e.status == 404 => Err(GqlError::new(
                    "NOT_FOUND",
                    format!("Could not resolve to a Repository with the name '{owner}/{repo}'."),
                )),
                Err(e) => Err(e.into()),
            }
        }
        "user" => {
            let login = a_str(args, "login").unwrap_or("");
            match cx.by_login(login).await {
                Ok(p) => Ok(actor(&p)),
                Err(_) => Err(GqlError::new(
                    "NOT_FOUND",
                    format!("Could not resolve to a User with the login of '{login}'."),
                )),
            }
        }
        "organization" => {
            let login = a_str(args, "login").unwrap_or("");
            org_out(cx, login).await.map_err(|_| {
                GqlError::new(
                    "NOT_FOUND",
                    format!("Could not resolve to an Organization with the login of '{login}'."),
                )
            })
        }
        "repositoryOwner" => {
            let login = a_str(args, "login").unwrap_or("");
            if let Ok(o) = org_out(cx, login).await {
                return Ok(o);
            }
            Ok(cx
                .by_login(login)
                .await
                .map(|p| actor(&p))
                .unwrap_or(Out::Null))
        }
        "node" => node_out(cx, a_str(args, "id").unwrap_or("")).await,
        "nodes" => {
            let mut out = Vec::new();
            for id in a_list(args, "ids") {
                out.push(node_out(cx, &id).await.unwrap_or(Out::Null));
            }
            Ok(Out::List(out))
        }
        "search" => search(ex, args).await,
        "rateLimit" => {
            let now = cx.hub.ports.clock.now_ms() / 1000;
            let limit = cx.hub.config.graphql_limit;
            let st = cx
                .hub
                .limiter
                .peek(&crate::http::rate_key(cx), "graphql", limit, now);
            Ok(Out::map(
                "RateLimit",
                vec![
                    ("cost", Out::leaf(1)),
                    ("limit", Out::leaf(st.limit)),
                    ("nodeCount", Out::leaf(0)),
                    ("remaining", Out::leaf(st.remaining())),
                    ("resetAt", Out::leaf(iso_secs(st.reset as i64))),
                    ("used", Out::leaf(st.used)),
                ],
            ))
        }
        _ => Ok(Out::Null),
    }
}

async fn org_out(cx: &Ctx, login: &str) -> Result<Out, GqlError> {
    let o = cx
        .hub
        .svc
        .get_org(cx.req(GetOrgRequest {
            name_or_id: login.to_string(),
        }))
        .await?
        .into_inner();
    Ok(Out::Obj(Node::Org(Arc::new(o))))
}

async fn node_out(cx: &Ctx, id: &str) -> GResult {
    let n = parse_node(id).ok_or_else(|| not_found(id))?;
    match n.kind.as_str() {
        "Repository" => Ok(Out::Obj(Node::Repo(Arc::new(
            repo_by_id(cx, &n.key).await?,
        )))),
        "Issue" | "PullRequest" => {
            let (repo, sub) = subject_of(cx, id).await?;
            Ok(subject_out(&repo, sub))
        }
        "User" | "Bot" => Ok(actor(&cx.principal(&n.key).await)),
        "Organization" => org_out(cx, &n.key).await,
        "Label" => {
            let (rid_, name) = n.key.split_once(':').ok_or_else(|| not_found(id))?;
            let repo = repo_by_id(cx, rid_).await?;
            let l = cx
                .hub
                .svc
                .get_issue_label(cx.req(bee_proto::v1::GetIssueLabelRequest {
                    repo: rid(&repo.id),
                    name: name.to_string(),
                }))
                .await?
                .into_inner();
            Ok(label_out(cx, &repo, &l))
        }
        "Milestone" => {
            let (repo, number) = numbered(cx, &n.key).await?;
            let m = cx
                .hub
                .svc
                .get_issue_milestone(cx.req(bee_proto::v1::GetIssueMilestoneRequest {
                    repo: rid(&repo.id),
                    number,
                }))
                .await?
                .into_inner();
            Ok(milestone_out(cx, &repo, &m))
        }
        _ => Err(not_found(id)),
    }
}

async fn search(ex: &Exec<'_>, args: &Args) -> GResult {
    let cx = &ex.cx;
    let q = a_str(args, "query").unwrap_or("");
    let ty = a_str(args, "type").unwrap_or("ISSUE");
    let zero = |n: usize, which: &str| {
        let mut v = vec![
            ("issueCount", Out::leaf(0)),
            ("repositoryCount", Out::leaf(0)),
            ("userCount", Out::leaf(0)),
            ("codeCount", Out::leaf(0)),
            ("discussionCount", Out::leaf(0)),
            ("wikiCount", Out::leaf(0)),
        ];
        for (k, val) in v.iter_mut() {
            if *k == which {
                *val = Out::leaf(n);
            }
        }
        v
    };
    match ty {
        "ISSUE" | "ISSUE_ADVANCED" => {
            let parsed = crate::rest::search::Query::parse(q);
            let sort = parsed
                .all("sort")
                .first()
                .map(|s| s.split('-').next().unwrap_or("created").to_string());
            let asc = parsed
                .all("sort")
                .first()
                .is_some_and(|s| s.ends_with("-asc"));
            let cleaned: String = q
                .split_whitespace()
                .filter(|t| !t.starts_with("sort:"))
                .collect::<Vec<_>>()
                .join(" ");
            let hits = crate::rest::search::issue_hits(cx, &cleaned, sort.as_deref(), asc).await?;
            let items: Vec<Out> = hits
                .into_iter()
                .map(|h| match h {
                    crate::rest::search::Hit::Issue(r, i) => {
                        Out::Obj(Node::Issue(Arc::new(r), Arc::new(*i)))
                    }
                    crate::rest::search::Hit::Pull(r, c) => {
                        Out::Obj(Node::Pull(Arc::new(r), Arc::new(*c)))
                    }
                })
                .collect();
            let n = items.len();
            Ok(connection(
                "SearchResultItemConnection",
                items,
                args,
                zero(n, "issueCount"),
            ))
        }
        "REPOSITORY" => {
            let repos = crate::rest::search::repo_hits(cx, q).await?;
            let items: Vec<Out> = repos
                .into_iter()
                .map(|r| Out::Obj(Node::Repo(Arc::new(r))))
                .collect();
            let n = items.len();
            Ok(connection(
                "SearchResultItemConnection",
                items,
                args,
                zero(n, "repositoryCount"),
            ))
        }
        _ => Ok(connection(
            "SearchResultItemConnection",
            Vec::new(),
            args,
            zero(0, "userCount"),
        )),
    }
}

// ----- shared objects -----------------------------------------------------------------------

fn reaction_word(c: ReactionContent) -> &'static str {
    match c {
        ReactionContent::ThumbsUp => "THUMBS_UP",
        ReactionContent::ThumbsDown => "THUMBS_DOWN",
        ReactionContent::Laugh => "LAUGH",
        ReactionContent::Hooray => "HOORAY",
        ReactionContent::Confused => "CONFUSED",
        ReactionContent::Heart => "HEART",
        ReactionContent::Rocket => "ROCKET",
        ReactionContent::Eyes => "EYES",
        ReactionContent::Unspecified => "",
    }
}

fn reaction_groups(groups: &[ReactionGroup]) -> Out {
    let all = [
        ReactionContent::ThumbsUp,
        ReactionContent::ThumbsDown,
        ReactionContent::Laugh,
        ReactionContent::Hooray,
        ReactionContent::Confused,
        ReactionContent::Heart,
        ReactionContent::Rocket,
        ReactionContent::Eyes,
    ];
    Out::List(
        all.iter()
            .map(|c| {
                let g = groups.iter().find(|g| g.content == *c as i32);
                let count = g.map(|g| g.count).unwrap_or(0);
                let users = Out::map(
                    "ReactingUserConnection",
                    vec![
                        ("totalCount", Out::leaf(count)),
                        ("nodes", Out::List(Vec::new())),
                        (
                            "pageInfo",
                            Out::map(
                                "PageInfo",
                                vec![
                                    ("hasNextPage", Out::leaf(false)),
                                    ("hasPreviousPage", Out::leaf(false)),
                                ],
                            ),
                        ),
                    ],
                );
                Out::map(
                    "ReactionGroup",
                    vec![
                        ("content", Out::leaf(reaction_word(*c))),
                        ("createdAt", Out::Null),
                        (
                            "viewerHasReacted",
                            Out::leaf(g.is_some_and(|g| g.viewer_reacted)),
                        ),
                        ("users", users.clone()),
                        ("reactors", users),
                    ],
                )
            })
            .collect(),
    )
}

fn label_out(cx: &Ctx, repo: &Repo, l: &Label) -> Out {
    let url = cx.html(&format!(
        "/{}/labels/{}",
        repo.full_name,
        crate::http::encode(&l.name)
    ));
    Out::map(
        "Label",
        vec![
            (
                "id",
                Out::leaf(node("Label", &format!("{}:{}", repo.id, l.name))),
            ),
            ("name", Out::leaf(l.name.clone())),
            ("color", Out::leaf(l.color.clone())),
            ("description", Out::opt_str(&l.description)),
            ("isDefault", Out::leaf(l.is_default)),
            ("url", Out::leaf(url)),
            (
                "resourcePath",
                Out::leaf(format!("/{}/labels/{}", repo.full_name, l.name)),
            ),
            ("createdAt", Out::Null),
            ("updatedAt", Out::Null),
            ("repository", Out::Obj(Node::Repo(Arc::new(repo.clone())))),
        ],
    )
}

fn milestone_out(cx: &Ctx, repo: &Repo, m: &Milestone) -> Out {
    if m.id.is_empty() && m.number == 0 {
        return Out::Null;
    }
    let closed = MilestoneState::try_from(m.state).unwrap_or_default() == MilestoneState::Closed;
    let total = (m.open_items + m.closed_items).max(1);
    Out::map(
        "Milestone",
        vec![
            (
                "id",
                Out::leaf(node("Milestone", &format!("{}:{}", repo.id, m.number))),
            ),
            ("number", Out::leaf(m.number)),
            ("title", Out::leaf(m.title.clone())),
            ("description", Out::opt_str(&m.description)),
            ("state", Out::leaf(if closed { "CLOSED" } else { "OPEN" })),
            ("dueOn", opt_ts(&m.due_on)),
            ("closed", Out::leaf(closed)),
            ("closedAt", opt_ts(&m.closed_at)),
            ("createdAt", opt_ts(&m.created_at)),
            ("updatedAt", Out::Leaf(iso_or(&m.closed_at, &m.created_at))),
            (
                "url",
                Out::leaf(cx.html(&format!("/{}/milestone/{}", repo.full_name, m.number))),
            ),
            (
                "resourcePath",
                Out::leaf(format!("/{}/milestone/{}", repo.full_name, m.number)),
            ),
            (
                "progressPercentage",
                Out::leaf(m.closed_items as f64 * 100.0 / total as f64),
            ),
            ("repository", Out::Obj(Node::Repo(Arc::new(repo.clone())))),
        ],
    )
}

fn association(a: &str) -> Out {
    Out::leaf(if a.is_empty() {
        "NONE".to_string()
    } else {
        a.to_string()
    })
}

fn comment_out(
    ex: &Exec<'_>,
    repo: &Repo,
    parent: Out,
    is_pr: bool,
    number: u64,
    c: &Comment,
) -> Out {
    let cx = &ex.cx;
    let id = cx
        .hub
        .ids
        .remember("comment", &c.id, &format!("{}:{number}", repo.id));
    let kind = if is_pr { "pull" } else { "issues" };
    let path = format!("/{}/{kind}/{number}#issuecomment-{id}", repo.full_name);
    let mine = c.author.as_ref().is_some_and(|a| a.id == cx.me());
    Out::map(
        "IssueComment",
        vec![
            ("id", Out::leaf(node("IssueComment", &c.id))),
            ("databaseId", Out::leaf(id)),
            ("author", c.author.as_ref().map(actor).unwrap_or_else(ghost)),
            ("authorAssociation", association(&c.author_association)),
            ("body", Out::leaf(c.body.clone())),
            ("bodyText", Out::leaf(c.body.clone())),
            ("bodyHTML", Out::leaf(c.body_html.clone())),
            ("createdAt", opt_ts(&c.created_at)),
            ("updatedAt", Out::Leaf(iso_or(&c.updated_at, &c.created_at))),
            (
                "lastEditedAt",
                if c.edited {
                    opt_ts(&c.updated_at)
                } else {
                    Out::Null
                },
            ),
            ("includesCreatedEdit", Out::leaf(c.edited)),
            ("isMinimized", Out::leaf(c.minimized_reason != 0)),
            ("minimizedReason", Out::Null),
            ("url", Out::leaf(cx.html(&path))),
            ("resourcePath", Out::leaf(path)),
            ("viewerDidAuthor", Out::leaf(mine)),
            ("viewerCanUpdate", Out::leaf(mine)),
            ("viewerCanDelete", Out::leaf(mine)),
            ("reactionGroups", reaction_groups(&c.reactions)),
            ("issue", if is_pr { Out::Null } else { parent.clone() }),
            ("pullRequest", if is_pr { parent } else { Out::Null }),
        ],
    )
}

fn permission_word(r: RepoRole) -> &'static str {
    match r {
        RepoRole::Admin => "ADMIN",
        RepoRole::Maintain => "MAINTAIN",
        RepoRole::Write => "WRITE",
        RepoRole::Triage => "TRIAGE",
        _ => "READ",
    }
}

async fn ref_out(cx: &Ctx, repo: &Repo, full: &str, sha: &str) -> Out {
    let (prefix, name) = if let Some(n) = full.strip_prefix("refs/heads/") {
        ("refs/heads/", n)
    } else if let Some(n) = full.strip_prefix("refs/tags/") {
        ("refs/tags/", n)
    } else {
        ("refs/", full.trim_start_matches("refs/"))
    };
    let target = match cx.hub.ports.git.commit(repo, sha).await.ok().flatten() {
        Some(c) => Out::Obj(Node::Commit(Arc::new(repo.clone()), Arc::new(c), None)),
        None => Out::Null,
    };
    let (rule, update_rule) = protection_rules(cx, repo, prefix, name).await;
    Out::map(
        "Ref",
        vec![
            ("id", Out::leaf(node("Ref", &format!("{}:{full}", repo.id)))),
            ("name", Out::leaf(name)),
            ("prefix", Out::leaf(prefix)),
            ("target", target),
            ("repository", Out::Obj(Node::Repo(Arc::new(repo.clone())))),
            ("branchProtectionRule", rule),
            ("refUpdateRule", update_rule),
            (
                "associatedPullRequests",
                connection("PullRequestConnection", Vec::new(), &Map::new(), Vec::new()),
            ),
        ],
    )
}

/// `branchProtectionRule` and `refUpdateRule` of a branch, from the repo policy's land rule.
async fn protection_rules(cx: &Ctx, repo: &Repo, prefix: &str, name: &str) -> (Out, Out) {
    if prefix != "refs/heads/" {
        return (Out::Null, Out::Null);
    }
    let Ok(Some(p)) = crate::rest::git::protection_of(cx, repo, name).await else {
        return (Out::Null, Out::Null);
    };
    let flag = |k: &str| {
        Out::leaf(
            p.pointer(&format!("/{k}/enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    };
    let contexts = p
        .pointer("/required_status_checks/contexts")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let has_checks = contexts.as_array().is_some_and(|a| !a.is_empty());
    let reviews = p
        .get("required_pull_request_reviews")
        .cloned()
        .unwrap_or(Value::Null);
    let count = reviews
        .get("required_approving_review_count")
        .and_then(Value::as_u64);
    let rb = |k: &str| Out::leaf(reviews.get(k).and_then(Value::as_bool).unwrap_or(false));
    let rule = Out::map(
        "BranchProtectionRule",
        vec![
            (
                "id",
                Out::leaf(node("BranchProtectionRule", &format!("{}:{name}", repo.id))),
            ),
            ("pattern", Out::leaf(name)),
            ("requiresStrictStatusChecks", Out::leaf(has_checks)),
            ("requiresStatusChecks", Out::leaf(has_checks)),
            ("requiredStatusCheckContexts", Out::Leaf(contexts.clone())),
            (
                "requiresApprovingReviews",
                Out::leaf(count.unwrap_or(0) > 0),
            ),
            (
                "requiredApprovingReviewCount",
                count.map(Out::leaf).unwrap_or(Out::Null),
            ),
            ("dismissesStaleReviews", rb("dismiss_stale_reviews")),
            ("requiresCodeOwnerReviews", rb("require_code_owner_reviews")),
            ("requireLastPushApproval", rb("require_last_push_approval")),
            ("requiresLinearHistory", flag("required_linear_history")),
            ("requiresCommitSignatures", flag("required_signatures")),
            ("isAdminEnforced", Out::leaf(true)),
            ("lockBranch", flag("lock_branch")),
            ("allowsForcePushes", Out::leaf(false)),
            ("allowsDeletions", Out::leaf(false)),
        ],
    );
    let update = Out::map(
        "RefUpdateRule",
        vec![
            ("pattern", Out::leaf(name)),
            (
                "requiredApprovingReviewCount",
                count.map(Out::leaf).unwrap_or(Out::Null),
            ),
            ("requiredStatusCheckContexts", Out::Leaf(contexts)),
            ("requiresLinearHistory", flag("required_linear_history")),
            ("requiresSignatures", flag("required_signatures")),
            ("viewerCanPush", Out::leaf(false)),
            ("allowsForcePushes", Out::leaf(false)),
            ("allowsDeletions", Out::leaf(false)),
        ],
    );
    (rule, update)
}

// ----- users and orgs -----------------------------------------------------------------------

async fn actor_field(ex: &Exec<'_>, p: &Principal, name: &str, args: &Args) -> GResult {
    let cx = &ex.cx;
    let login = cx.login(p).await;
    let is_bot = matches!(
        bee_proto::v1::PrincipalKind::try_from(p.kind).unwrap_or_default(),
        bee_proto::v1::PrincipalKind::Agent | bee_proto::v1::PrincipalKind::Service
    );
    let profile = || async {
        cx.hub
            .svc
            .get_user(cx.req(bee_proto::v1::GetUserRequest {
                user: Some(bee_proto::v1::get_user_request::User::Id(p.id.clone())),
            }))
            .await
            .ok()
            .map(|r| r.into_inner())
    };
    Ok(match name {
        "id" => Out::leaf(node(if is_bot { "Bot" } else { "User" }, &p.id)),
        "databaseId" => Out::leaf(cx.hub.ids.remember("user", &p.id, "")),
        "login" => Out::leaf(login),
        "name" => Out::opt_str(&p.display),
        "email" => Out::leaf(
            cx.hub
                .forge
                .hooks
                .directory
                .identities(&p.id)
                .await
                .1
                .first()
                .cloned()
                .unwrap_or_default(),
        ),
        "avatarUrl" => Out::leaf(cx.html(&format!("/{login}.png"))),
        "url" => Out::leaf(cx.html(&format!("/{login}"))),
        "resourcePath" => Out::leaf(format!("/{login}")),
        "bio" | "company" | "location" | "websiteUrl" => match profile().await {
            Some(pr) => Out::opt_str(match name {
                "bio" => &pr.bio,
                "company" => &pr.company,
                "location" => &pr.location,
                _ => &pr.website,
            }),
            None => Out::Null,
        },
        "createdAt" | "updatedAt" => match profile().await {
            Some(pr) => Out::Leaf(iso_or(&pr.created_at, &Some(crate::time::ts_secs(0)))),
            None => Out::leaf(iso_secs(0)),
        },
        "isSiteAdmin" | "isBountyHunter" => Out::leaf(false),
        "isViewer" => Out::leaf(p.id == cx.me()),
        "twitterUsername" | "status" => Out::Null,
        "repository" => {
            let n = a_str(args, "name").unwrap_or("");
            match crate::rest::repo_named(cx, &login, n).await {
                Ok(r) => Out::Obj(Node::Repo(Arc::new(r))),
                Err(_) => Out::Null,
            }
        }
        "repositories" => {
            let repos = crate::rest::repos::list_org(cx, &login)
                .await
                .unwrap_or_default();
            repos_connection(repos, args)
        }
        "organizations" => {
            let orgs = crate::rest::repos::orgs_of(cx, &p.id)
                .await
                .unwrap_or_default();
            let items = orgs
                .into_iter()
                .map(|o| Out::Obj(Node::Org(Arc::new(o))))
                .collect();
            connection("OrganizationConnection", items, args, Vec::new())
        }
        "organization" => org_out(cx, a_str(args, "login").unwrap_or(""))
            .await
            .unwrap_or(Out::Null),
        _ => Out::Null,
    })
}

fn repos_connection(mut repos: Vec<Repo>, args: &Args) -> Out {
    if let Some(p) = a_str(args, "privacy") {
        repos.retain(|r| (shapes::visibility(r) == "public") == (p == "PUBLIC"));
    }
    if let Some(f) = a_bool(args, "isFork") {
        repos.retain(|r| r.fork_parent_id.is_empty() != f);
    }
    if let Some(a) = a_bool(args, "isArchived") {
        repos.retain(|r| r.archived == a);
    }
    let order = args.get("orderBy").cloned().unwrap_or(Value::Null);
    let field = order.get("field").and_then(Value::as_str).unwrap_or("NAME");
    let desc = order.get("direction").and_then(Value::as_str) == Some("DESC");
    repos.sort_by(|a, b| {
        let o = match field {
            "CREATED_AT" => a
                .created_at
                .as_ref()
                .map(|t| t.seconds)
                .cmp(&b.created_at.as_ref().map(|t| t.seconds)),
            "PUSHED_AT" | "UPDATED_AT" => a
                .pushed_at
                .as_ref()
                .map(|t| t.seconds)
                .cmp(&b.pushed_at.as_ref().map(|t| t.seconds)),
            "STARGAZERS" => a.stars.cmp(&b.stars),
            _ => a.name.cmp(&b.name),
        };
        if desc { o.reverse() } else { o }
    });
    let disk: u64 = repos.iter().map(|r| r.size_bytes / 1024).sum();
    let items = repos
        .into_iter()
        .map(|r| Out::Obj(Node::Repo(Arc::new(r))))
        .collect();
    connection(
        "RepositoryConnection",
        items,
        args,
        vec![("totalDiskUsage", Out::leaf(disk))],
    )
}

async fn org_field(ex: &Exec<'_>, o: &Org, name: &str, args: &Args) -> GResult {
    let cx = &ex.cx;
    Ok(match name {
        "id" => Out::leaf(node("Organization", &o.id)),
        "databaseId" => Out::leaf(cx.hub.ids.remember("org", &o.id, &o.name)),
        "login" => Out::leaf(o.name.clone()),
        "name" => Out::leaf(if o.display.is_empty() {
            o.name.clone()
        } else {
            o.display.clone()
        }),
        "description" => Out::opt_str(&o.description),
        "email" => Out::opt_str(&o.email),
        "avatarUrl" => Out::leaf(if o.avatar_url.is_empty() {
            cx.html(&format!("/{}.png", o.name))
        } else {
            o.avatar_url.clone()
        }),
        "url" => Out::leaf(cx.html(&format!("/{}", o.name))),
        "resourcePath" => Out::leaf(format!("/{}", o.name)),
        "websiteUrl" => Out::opt_str(&o.website),
        "location" => Out::opt_str(&o.location),
        "createdAt" => opt_ts(&o.created_at),
        "viewerIsAMember" => Out::leaf(
            crate::rest::repos::orgs_of(cx, "")
                .await
                .unwrap_or_default()
                .iter()
                .any(|x| x.id == o.id),
        ),
        "repository" => {
            match crate::rest::repo_named(cx, &o.name, a_str(args, "name").unwrap_or("")).await {
                Ok(r) => Out::Obj(Node::Repo(Arc::new(r))),
                Err(_) => Out::Null,
            }
        }
        "repositories" => repos_connection(
            crate::rest::repos::list_org(cx, &o.name)
                .await
                .unwrap_or_default(),
            args,
        ),
        "teams" => {
            let teams = match &cx.hub.teams {
                Some(api) => {
                    let api = api.clone();
                    all_pages(|page| {
                        let api = api.clone();
                        let req = cx.req(ListTeamsRequest {
                            org: o.name.clone(),
                            query: a_str(args, "query").unwrap_or("").to_string(),
                            top_level_only: false,
                            page: Some(page),
                        });
                        async move {
                            let r = api.list_teams(req).await?.into_inner();
                            Ok((r.teams, r.page))
                        }
                    })
                    .await
                    .unwrap_or_default()
                }
                None => Vec::new(),
            };
            let items = teams.iter().map(|t| team_out(cx, o, t)).collect();
            connection("TeamConnection", items, args, Vec::new())
        }
        "team" => match &cx.hub.teams {
            Some(api) => match api
                .get_team(cx.req(GetTeamRequest {
                    org: o.name.clone(),
                    team: a_str(args, "slug").unwrap_or("").to_string(),
                }))
                .await
            {
                Ok(t) => team_out(cx, o, &t.into_inner()),
                Err(_) => Out::Null,
            },
            None => Out::Null,
        },
        _ => Out::Null,
    })
}

fn team_out(cx: &Ctx, o: &Org, t: &bee_proto::v1::Team) -> Out {
    Out::map(
        "Team",
        vec![
            ("id", Out::leaf(node("Team", &t.id))),
            ("databaseId", Out::leaf(num("team", &t.id))),
            ("name", Out::leaf(t.name.clone())),
            ("slug", Out::leaf(t.slug.clone())),
            ("combinedSlug", Out::leaf(format!("{}/{}", t.org, t.slug))),
            ("description", Out::opt_str(&t.description)),
            ("organization", Out::Obj(Node::Org(Arc::new(o.clone())))),
            (
                "url",
                Out::leaf(cx.html(&format!("/orgs/{}/teams/{}", t.org, t.slug))),
            ),
        ],
    )
}

// ----- repository ---------------------------------------------------------------------------

async fn repo_field(ex: &Exec<'_>, r: &Arc<Repo>, name: &str, args: &Args) -> GResult {
    let cx = &ex.cx;
    let repo: &Repo = r;
    let (owner, rname) = shapes::split(repo);
    let html = cx.html(&format!("/{}", repo.full_name));
    let settings = || crate::rest::settings(cx, repo);
    let allowed = |s: &Option<bee_proto::v1::RepoSettings>, m: LandMethod| {
        s.as_ref().is_none_or(|s| {
            s.allowed_land_methods.is_empty() || s.allowed_land_methods.contains(&(m as i32))
        })
    };
    let default_branch = if repo.default_ref.is_empty() {
        "main".to_string()
    } else {
        repo.default_ref.clone()
    };
    Ok(match name {
        "id" => Out::leaf(node("Repository", &repo.id)),
        "databaseId" => Out::leaf(cx.hub.ids.remember("repo", &repo.id, &repo.full_name)),
        "name" => Out::leaf(rname),
        "nameWithOwner" => Out::leaf(repo.full_name.clone()),
        "owner" => match org_out(cx, owner).await {
            Ok(o) => o,
            Err(_) => Out::map(
                "Organization",
                vec![
                    ("login", Out::leaf(owner)),
                    ("id", Out::leaf(node("Organization", &repo.org_id))),
                ],
            ),
        },
        "description" => Out::opt_str(&repo.description),
        "descriptionHTML" => Out::leaf(repo.description.clone()),
        "url" => Out::leaf(html.clone()),
        "resourcePath" => Out::leaf(format!("/{}", repo.full_name)),
        "homepageUrl" => Out::opt_str(&repo.homepage),
        "sshUrl" => Out::leaf(format!("{html}.git")),
        "createdAt" => opt_ts(&repo.created_at),
        "updatedAt" | "pushedAt" => Out::Leaf(iso_or(&repo.pushed_at, &repo.created_at)),
        "archivedAt" => Out::Null,
        "isPrivate" => Out::leaf(shapes::visibility(repo) != "public"),
        "visibility" => Out::leaf(
            match Visibility::try_from(repo.visibility).unwrap_or_default() {
                Visibility::Public => "PUBLIC",
                Visibility::Internal => "INTERNAL",
                _ => "PRIVATE",
            },
        ),
        "isArchived" => Out::leaf(repo.archived),
        "isFork" => Out::leaf(!repo.fork_parent_id.is_empty()),
        "isTemplate" => Out::leaf(repo.is_template),
        "isEmpty" => Out::leaf(
            cx.hub
                .ports
                .git
                .refs(repo, "refs/heads/")
                .await
                .map(|r| r.is_empty())
                .unwrap_or(true),
        ),
        "isMirror" | "isLocked" | "isDisabled" | "isUserConfigurationRepository" => {
            Out::leaf(false)
        }
        "isInOrganization" => Out::leaf(true),
        "isSecurityPolicyEnabled" => Out::leaf(false),
        "hasIssuesEnabled" => Out::leaf(settings().await.map(|s| s.has_issues).unwrap_or(true)),
        "hasWikiEnabled" => Out::leaf(settings().await.map(|s| s.has_wiki).unwrap_or(false)),
        "hasProjectsEnabled" => Out::leaf(settings().await.map(|s| s.has_projects).unwrap_or(true)),
        "hasDiscussionsEnabled" => {
            Out::leaf(settings().await.map(|s| s.has_discussions).unwrap_or(false))
        }
        "forkingAllowed" => Out::leaf(settings().await.map(|s| s.allow_forking).unwrap_or(true)),
        "mergeCommitAllowed" => Out::leaf(allowed(&settings().await, LandMethod::MergeCommit)),
        "rebaseMergeAllowed" => Out::leaf(allowed(&settings().await, LandMethod::Rebase)),
        "squashMergeAllowed" => Out::leaf(allowed(&settings().await, LandMethod::Squash)),
        "autoMergeAllowed" => Out::leaf(
            settings()
                .await
                .map(|s| s.allow_auto_merge)
                .unwrap_or(false),
        ),
        "deleteBranchOnMerge" => Out::leaf(
            settings()
                .await
                .map(|s| s.delete_workspace_ref_on_land)
                .unwrap_or(false),
        ),
        "viewerDefaultMergeMethod" => {
            let s = settings().await;
            let m = s
                .as_ref()
                .map(|s| LandMethod::try_from(s.default_land_method).unwrap_or_default())
                .unwrap_or(LandMethod::MergeCommit);
            Out::leaf(match m {
                LandMethod::Squash => "SQUASH",
                LandMethod::Rebase => "REBASE",
                _ => "MERGE",
            })
        }
        "viewerDefaultCommitEmail" => Out::Null,
        "viewerPermission" => match crate::rest::my_role(cx, repo).await {
            Some(r) if r as i32 >= RepoRole::Read as i32 => Out::leaf(permission_word(r)),
            _ => Out::Null,
        },
        "viewerCanAdminister" => {
            Out::leaf(crate::rest::my_role(cx, repo).await == Some(RepoRole::Admin))
        }
        "viewerCanUpdateTopics" => Out::leaf(
            crate::rest::my_role(cx, repo)
                .await
                .is_some_and(|r| r as i32 >= RepoRole::Maintain as i32),
        ),
        "viewerHasStarred" => Out::leaf(
            cx.hub
                .svc
                .is_repo_starred(cx.req(bee_proto::v1::IsRepoStarredRequest {
                    repo: rid(&repo.id),
                }))
                .await
                .map(|r| r.into_inner().starred)
                .unwrap_or(false),
        ),
        "viewerSubscription" => Out::leaf("UNSUBSCRIBED"),
        "stargazerCount" => Out::leaf(repo.stars),
        "forkCount" => Out::leaf(repo.forks),
        "diskUsage" => Out::leaf(repo.size_bytes / 1024),
        "securityPolicyUrl" | "mirrorUrl" | "templateRepository" | "codeOfConduct" => Out::Null,
        "parent" => {
            if repo.fork_parent_id.is_empty() {
                Out::Null
            } else {
                repo_by_id(cx, &repo.fork_parent_id)
                    .await
                    .map(|p| Out::Obj(Node::Repo(Arc::new(p))))
                    .unwrap_or(Out::Null)
            }
        }
        "defaultBranchRef" => match cx.hub.ports.git.resolve(repo, &default_branch).await? {
            Some(sha) => ref_out(cx, repo, &format!("refs/heads/{default_branch}"), &sha).await,
            None => Out::Null,
        },
        "primaryLanguage" => {
            if repo.primary_language.is_empty() {
                Out::Null
            } else {
                Out::map(
                    "Language",
                    vec![
                        ("id", Out::leaf(node("Language", &repo.primary_language))),
                        ("name", Out::leaf(repo.primary_language.clone())),
                        ("color", Out::Null),
                    ],
                )
            }
        }
        "languages" => {
            let items = if repo.primary_language.is_empty() {
                Vec::new()
            } else {
                vec![Out::map(
                    "Language",
                    vec![
                        ("id", Out::leaf(node("Language", &repo.primary_language))),
                        ("name", Out::leaf(repo.primary_language.clone())),
                        ("color", Out::Null),
                    ],
                )]
            };
            let size = repo.size_bytes;
            let conn = connection(
                "LanguageConnection",
                items,
                args,
                vec![("totalSize", Out::leaf(size))],
            );
            // Language edges carry a size.
            if let Out::Obj(Node::Map(t, m)) = conn {
                let mut m = (*m).clone();
                if let Some(Out::List(edges)) = m.get("edges").cloned() {
                    let sized = edges
                        .into_iter()
                        .map(|e| match e {
                            Out::Obj(Node::Map(et, em)) => {
                                let mut em = (*em).clone();
                                em.insert("size".into(), Out::leaf(size));
                                Out::Obj(Node::Map(et, Arc::new(em)))
                            }
                            other => other,
                        })
                        .collect();
                    m.insert("edges".into(), Out::List(sized));
                }
                Out::Obj(Node::Map(t, Arc::new(m)))
            } else {
                conn
            }
        }
        "licenseInfo" => {
            if repo.license_spdx.is_empty() {
                Out::Null
            } else {
                Out::map(
                    "License",
                    vec![
                        ("id", Out::leaf(node("License", &repo.license_spdx))),
                        ("key", Out::leaf(repo.license_spdx.to_lowercase())),
                        ("name", Out::leaf(repo.license_spdx.clone())),
                        ("nickname", Out::Null),
                        ("spdxId", Out::leaf(repo.license_spdx.clone())),
                        ("url", Out::Null),
                    ],
                )
            }
        }
        "fundingLinks" | "issueTemplates" | "pullRequestTemplates" => Out::List(Vec::new()),
        "latestRelease" => match cx
            .hub
            .svc
            .get_latest_release(cx.req(GetLatestReleaseRequest {
                repo: rid(&repo.id),
                include_html: false,
            }))
            .await
        {
            Ok(rel) => Out::Obj(Node::Release(r.clone(), Arc::new(rel.into_inner()))),
            Err(_) => Out::Null,
        },
        "releases" => {
            let svc = cx.hub.svc.clone();
            let mut list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListReleasesRequest {
                    repo: rid(&repo.id),
                    include_drafts: true,
                    include_prereleases: true,
                    page: Some(page),
                });
                async move {
                    let x = svc.list_releases(req).await?.into_inner();
                    Ok((x.releases, x.page))
                }
            })
            .await?;
            list.sort_by(|a, b| {
                b.created_at
                    .as_ref()
                    .map(|t| (t.seconds, t.nanos))
                    .cmp(&a.created_at.as_ref().map(|t| (t.seconds, t.nanos)))
            });
            let items = list
                .into_iter()
                .map(|x| Out::Obj(Node::Release(r.clone(), Arc::new(x))))
                .collect();
            connection("ReleaseConnection", items, args, Vec::new())
        }
        "release" => match cx
            .hub
            .svc
            .get_release_by_tag(cx.req(bee_proto::v1::GetReleaseByTagRequest {
                repo: rid(&repo.id),
                tag_name: a_str(args, "tagName").unwrap_or("").to_string(),
                include_html: false,
            }))
            .await
        {
            Ok(rel) => Out::Obj(Node::Release(r.clone(), Arc::new(rel.into_inner()))),
            Err(_) => Out::Null,
        },
        "watchers" | "mentionableUsers" | "assignableUsers" => {
            let svc = cx.hub.svc.clone();
            let query = a_str(args, "query").unwrap_or("").to_string();
            let list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListIssueAssignableUsersRequest {
                    repo: rid(&repo.id),
                    query: query.clone(),
                    kind: 0,
                    page: Some(page),
                });
                async move {
                    let x = svc.list_issue_assignable_users(req).await?.into_inner();
                    Ok((x.principals, x.page))
                }
            })
            .await
            .unwrap_or_default();
            let items = list.iter().map(actor).collect();
            connection("UserConnection", items, args, Vec::new())
        }
        "stargazers" => connection("StargazerConnection", Vec::new(), args, Vec::new()),
        "collaborators" => {
            let svc = cx.hub.svc.clone();
            let list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListCollaboratorsRequest {
                    repo: rid(&repo.id),
                    direct_only: false,
                    page: Some(page),
                });
                async move {
                    let x = svc.list_collaborators(req).await?.into_inner();
                    Ok((x.collaborators, x.page))
                }
            })
            .await
            .unwrap_or_default();
            let items = list.iter().map(|c| actor_opt(&c.principal)).collect();
            connection("RepositoryCollaboratorConnection", items, args, Vec::new())
        }
        "repositoryTopics" => {
            let items = repo
                .topics
                .iter()
                .map(|t| {
                    Out::map(
                        "RepositoryTopic",
                        vec![
                            (
                                "id",
                                Out::leaf(node("RepositoryTopic", &format!("{}:{t}", repo.id))),
                            ),
                            (
                                "topic",
                                Out::map(
                                    "Topic",
                                    vec![
                                        ("id", Out::leaf(node("Topic", t))),
                                        ("name", Out::leaf(t.clone())),
                                    ],
                                ),
                            ),
                            ("url", Out::leaf(cx.html(&format!("/topics/{t}")))),
                            ("resourcePath", Out::leaf(format!("/topics/{t}"))),
                        ],
                    )
                })
                .collect();
            connection("RepositoryTopicConnection", items, args, Vec::new())
        }
        "issue" | "pullRequest" | "issueOrPullRequest" => {
            let n = a_u64(args, "number").unwrap_or(0);
            match crate::rest::issues::subject(cx, repo, n).await {
                Ok(Subject::Issue(i)) if name != "pullRequest" => {
                    Out::Obj(Node::Issue(r.clone(), Arc::new(*i)))
                }
                Ok(Subject::Pull(c)) if name != "issue" => {
                    Out::Obj(Node::Pull(r.clone(), Arc::new(*c)))
                }
                Ok(_) | Err(_) => {
                    let what = match name {
                        "issue" => "an Issue",
                        "pullRequest" => "a PullRequest",
                        _ => "an issue or pull request",
                    };
                    return Err(GqlError::new(
                        "NOT_FOUND",
                        format!("Could not resolve to {what} with the number of {n}."),
                    ));
                }
            }
        }
        "issues" => issues_connection(ex, r, args).await?,
        "pullRequests" => pulls_connection(ex, r, args).await?,
        "labels" => {
            let svc = cx.hub.svc.clone();
            let query = a_str(args, "query").unwrap_or("").to_string();
            let mut list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListIssueLabelsRequest {
                    repo: rid(&repo.id),
                    query: query.clone(),
                    page: Some(page),
                });
                async move {
                    let x = svc.list_issue_labels(req).await?.into_inner();
                    Ok((x.labels, x.page))
                }
            })
            .await?;
            list.sort_by_key(|l| l.name.to_lowercase());
            let items = list.iter().map(|l| label_out(cx, repo, l)).collect();
            connection("LabelConnection", items, args, Vec::new())
        }
        "label" => {
            match cx
                .hub
                .svc
                .get_issue_label(cx.req(bee_proto::v1::GetIssueLabelRequest {
                    repo: rid(&repo.id),
                    name: a_str(args, "name").unwrap_or("").to_string(),
                }))
                .await
            {
                Ok(l) => label_out(cx, repo, &l.into_inner()),
                Err(_) => Out::Null,
            }
        }
        "milestones" => {
            let states = a_list(args, "states");
            let svc = cx.hub.svc.clone();
            let list = all_pages(|page| {
                let svc = svc.clone();
                let req = cx.req(ListIssueMilestonesRequest {
                    repo: rid(&repo.id),
                    state: if states == ["CLOSED"] {
                        MilestoneState::Closed as i32
                    } else {
                        MilestoneState::Open as i32
                    },
                    all_states: states.is_empty() || states.len() > 1,
                    page: Some(page),
                });
                async move {
                    let x = svc.list_issue_milestones(req).await?.into_inner();
                    Ok((x.milestones, x.page))
                }
            })
            .await?;
            let q = a_str(args, "query").unwrap_or("").to_lowercase();
            let items = list
                .iter()
                .filter(|m| m.title.to_lowercase().contains(&q))
                .map(|m| milestone_out(cx, repo, m))
                .collect();
            connection("MilestoneConnection", items, args, Vec::new())
        }
        "milestone" => match cx
            .hub
            .svc
            .get_issue_milestone(cx.req(bee_proto::v1::GetIssueMilestoneRequest {
                repo: rid(&repo.id),
                number: a_u64(args, "number").unwrap_or(0),
            }))
            .await
        {
            Ok(m) => milestone_out(cx, repo, &m.into_inner()),
            Err(_) => Out::Null,
        },
        "ref" => {
            let q = a_str(args, "qualifiedName").unwrap_or("");
            let full = if q.starts_with("refs/") {
                q.to_string()
            } else {
                format!("refs/heads/{q}")
            };
            match cx
                .hub
                .ports
                .git
                .refs(repo, &full)
                .await?
                .into_iter()
                .find(|x| x.name == full)
            {
                Some(x) => ref_out(cx, repo, &x.name, &x.sha).await,
                None => Out::Null,
            }
        }
        "refs" => {
            let prefix = a_str(args, "refPrefix").unwrap_or("refs/heads/");
            let query = a_str(args, "query").unwrap_or("");
            let mut items = Vec::new();
            for x in cx.hub.ports.git.refs(repo, prefix).await? {
                if x.name[prefix.len().min(x.name.len())..].contains(query) {
                    items.push(ref_out(cx, repo, &x.name, &x.sha).await);
                }
            }
            connection("RefConnection", items, args, Vec::new())
        }
        "object" => {
            let expr = a_str(args, "expression")
                .or(a_str(args, "oid"))
                .unwrap_or("");
            object_out(cx, r, expr).await?
        }
        "projects" => empty_connection("ProjectConnection", args),
        "projectsV2" => empty_connection("ProjectV2Connection", args),
        _ => Out::Null,
    })
}

async fn object_out(cx: &Ctx, r: &Arc<Repo>, expr: &str) -> GResult {
    let git = &cx.hub.ports.git;
    let (rev, path) = match expr.split_once(':') {
        Some((a, b)) => (if a.is_empty() { "HEAD" } else { a }, Some(b)),
        None => (expr, None),
    };
    let Some(sha) = git.resolve(r, rev).await? else {
        return Ok(Out::Null);
    };
    let Some(path) = path else {
        return Ok(match git.commit(r, &sha).await? {
            Some(c) => Out::Obj(Node::Commit(r.clone(), Arc::new(c), None)),
            None => Out::Null,
        });
    };
    let base = |id: &str, ty: &str, oid: &str| {
        vec![
            ("id", Out::leaf(node(ty, &format!("{id}:{oid}")))),
            ("oid", Out::leaf(oid.to_string())),
            (
                "abbreviatedOid",
                Out::leaf(oid.chars().take(7).collect::<String>()),
            ),
            (
                "commitUrl",
                Out::leaf(cx.html(&format!("/{}/commit/{sha}", r.full_name))),
            ),
            ("repository", Out::Obj(Node::Repo(r.clone()))),
        ]
    };
    Ok(match git.path(r, &sha, path).await? {
        Some(PathEntry::File { sha: oid, bytes }) => {
            let mut f = base(&r.id, "Blob", &oid);
            let text = String::from_utf8(bytes.clone()).ok();
            f.push(("byteSize", Out::leaf(bytes.len())));
            f.push(("isBinary", Out::leaf(text.is_none())));
            f.push(("isTruncated", Out::leaf(false)));
            f.push(("text", text.map(Out::leaf).unwrap_or(Out::Null)));
            Out::map("Blob", f)
        }
        Some(PathEntry::Dir { sha: oid, entries }) => {
            let mut f = base(&r.id, "Tree", &oid);
            f.push((
                "entries",
                Out::List(
                    entries
                        .iter()
                        .map(|e| {
                            Out::map(
                                "TreeEntry",
                                vec![
                                    ("name", Out::leaf(e.path.clone())),
                                    (
                                        "path",
                                        Out::leaf(if path.is_empty() {
                                            e.path.clone()
                                        } else {
                                            format!("{path}/{}", e.path)
                                        }),
                                    ),
                                    (
                                        "mode",
                                        Out::leaf(i64::from_str_radix(&e.mode, 8).unwrap_or(0)),
                                    ),
                                    ("type", Out::leaf(e.kind.as_str())),
                                    ("oid", Out::leaf(e.sha.clone())),
                                    ("extension", Out::Null),
                                    ("isGenerated", Out::leaf(false)),
                                ],
                            )
                        })
                        .collect(),
                ),
            ));
            Out::map("Tree", f)
        }
        _ => Out::Null,
    })
}

fn order_of(args: &Args) -> (String, bool) {
    let o = args.get("orderBy").cloned().unwrap_or(Value::Null);
    (
        o.get("field")
            .and_then(Value::as_str)
            .unwrap_or("CREATED_AT")
            .to_string(),
        o.get("direction").and_then(Value::as_str).unwrap_or("DESC") == "DESC",
    )
}

async fn pulls_connection(ex: &Exec<'_>, r: &Arc<Repo>, args: &Args) -> GResult {
    let cx = &ex.cx;
    if cx.hub.changes.is_none() {
        return Ok(empty_connection("PullRequestConnection", args));
    }
    let states = a_list(args, "states");
    let open_only = states == ["OPEN"];
    let mut list = pulls::list_crs(
        cx,
        r,
        if open_only { "open" } else { "all" },
        a_str(args, "baseRefName"),
        "",
    )
    .await?;
    if !states.is_empty() {
        list.retain(|c| {
            let w = match shapes::cr_state(c) {
                ("open", _) => "OPEN",
                (_, true) => "MERGED",
                _ => "CLOSED",
            };
            states.iter().any(|s| s == w)
        });
    }
    let labels = a_list(args, "labels");
    if !labels.is_empty() {
        list.retain(|c| {
            labels
                .iter()
                .all(|l| c.labels.iter().any(|x| x.name.eq_ignore_ascii_case(l)))
        });
    }
    if let Some(h) = a_str(args, "headRefName") {
        let mut keep = Vec::new();
        for c in list {
            if pulls::head_ref(cx, r, &c).await == h {
                keep.push(c);
            }
        }
        list = keep;
    }
    let (field, desc) = order_of(args);
    list.sort_by(|a, b| {
        let k = |c: &ChangeRequest| match field.as_str() {
            "UPDATED_AT" => c.updated_at.as_ref().map(|t| t.seconds).unwrap_or(0),
            "COMMENTS" => c.comment_count as i64,
            _ => c.created_at.as_ref().map(|t| t.seconds).unwrap_or(0),
        };
        let o = (k(a), a.number).cmp(&(k(b), b.number));
        if desc { o.reverse() } else { o }
    });
    let items = list
        .into_iter()
        .map(|c| Out::Obj(Node::Pull(r.clone(), Arc::new(c))))
        .collect();
    Ok(connection("PullRequestConnection", items, args, Vec::new()))
}

async fn issues_connection(ex: &Exec<'_>, r: &Arc<Repo>, args: &Args) -> GResult {
    let cx = &ex.cx;
    let filter = args.get("filterBy").cloned().unwrap_or(Value::Null);
    let mut states = a_list(args, "states");
    if states.is_empty()
        && let Some(Value::Array(a)) = filter.get("states")
    {
        states = a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    let all = states.is_empty() || states.len() > 1;
    let mut labels = a_list(args, "labels");
    if let Some(Value::Array(a)) = filter.get("labels") {
        labels.extend(a.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    let who = |k: &str| filter.get(k).and_then(Value::as_str).map(str::to_string);
    let resolve_login = |l: Option<String>| async move {
        match l {
            Some(l) if l != "none" && l != "*" => cx.by_login(&l).await.map(|p| p.id).unwrap_or(l),
            Some(l) => l,
            None => String::new(),
        }
    };
    let assignee = resolve_login(who("assignee")).await;
    let author = resolve_login(who("createdBy")).await;
    let mentioned = who("mentioned").unwrap_or_default();
    let milestone = who("milestoneNumber")
        .or_else(|| who("milestone"))
        .unwrap_or_default();
    let since = filter
        .get("since")
        .and_then(Value::as_str)
        .and_then(crate::time::parse_iso)
        .map(crate::time::ts_secs);
    let svc = cx.hub.svc.clone();
    let mut list = all_pages(|page| {
        let svc = svc.clone();
        let req = cx.req(bee_proto::v1::ListIssuesRequest {
            repo: rid(&r.id),
            state: if states == ["CLOSED"] {
                IssueState::Closed as i32
            } else {
                IssueState::Open as i32
            },
            all_states: all,
            labels: labels.clone(),
            assignee: assignee.clone(),
            author: author.clone(),
            mentioned: mentioned.clone(),
            milestone: milestone.clone(),
            since,
            page: Some(page),
            ..Default::default()
        });
        async move {
            let x = svc.list_issues(req).await?.into_inner();
            Ok((x.issues, x.page))
        }
    })
    .await?;
    let (field, desc) = order_of(args);
    list.sort_by(|a, b| {
        let k = |i: &Issue| match field.as_str() {
            "UPDATED_AT" => i.updated_at.as_ref().map(|t| t.seconds).unwrap_or(0),
            "COMMENTS" => i.comment_count as i64,
            _ => i.created_at.as_ref().map(|t| t.seconds).unwrap_or(0),
        };
        let o = (k(a), a.number).cmp(&(k(b), b.number));
        if desc { o.reverse() } else { o }
    });
    let items = list
        .into_iter()
        .map(|i| Out::Obj(Node::Issue(r.clone(), Arc::new(i))))
        .collect();
    Ok(connection("IssueConnection", items, args, Vec::new()))
}

// ----- issues and pull requests -------------------------------------------------------------

async fn labels_assignees_common(
    ex: &Exec<'_>,
    repo: &Repo,
    name: &str,
    args: &Args,
    labels: &[Label],
    assignees: &[Principal],
) -> Option<Out> {
    let cx = &ex.cx;
    Some(match name {
        "labels" => connection(
            "LabelConnection",
            labels.iter().map(|l| label_out(cx, repo, l)).collect(),
            args,
            Vec::new(),
        ),
        "assignees" => connection(
            "UserConnection",
            assignees.iter().map(actor).collect(),
            args,
            Vec::new(),
        ),
        "projectCards" => empty_connection("ProjectCardConnection", args),
        "projectItems" => empty_connection("ProjectV2ItemConnection", args),
        "projectsV2" => empty_connection("ProjectV2Connection", args),
        _ => return None,
    })
}

async fn issue_field(
    ex: &Exec<'_>,
    r: &Arc<Repo>,
    i: &Arc<Issue>,
    name: &str,
    args: &Args,
) -> GResult {
    let cx = &ex.cx;
    let repo: &Repo = r;
    if let Some(o) = labels_assignees_common(ex, repo, name, args, &i.labels, &i.assignees).await {
        return Ok(o);
    }
    let closed = IssueState::try_from(i.state).unwrap_or_default() == IssueState::Closed;
    let path = format!("/{}/issues/{}", repo.full_name, i.number);
    let me = cx.me();
    Ok(match name {
        "id" => Out::leaf(node("Issue", &format!("{}:{}", repo.id, i.number))),
        "databaseId" => Out::leaf(cx.hub.ids.remember(
            "issue",
            &i.id,
            &format!("{}:{}", repo.id, i.number),
        )),
        "number" => Out::leaf(i.number),
        "title" => Out::leaf(i.title.clone()),
        "body" | "bodyText" => Out::leaf(i.body.clone()),
        "bodyHTML" => Out::leaf(i.body_html.clone()),
        "url" => Out::leaf(cx.html(&path)),
        "resourcePath" => Out::leaf(path),
        "state" => Out::leaf(if closed { "CLOSED" } else { "OPEN" }),
        "stateReason" => match IssueStateReason::try_from(i.state_reason).unwrap_or_default() {
            IssueStateReason::Completed => Out::leaf("COMPLETED"),
            IssueStateReason::NotPlanned => Out::leaf("NOT_PLANNED"),
            IssueStateReason::Duplicate => Out::leaf("DUPLICATE"),
            IssueStateReason::Reopened => Out::leaf("REOPENED"),
            IssueStateReason::Unspecified => Out::Null,
        },
        "closed" => Out::leaf(closed),
        "closedAt" => opt_ts(&i.closed_at),
        "createdAt" => opt_ts(&i.created_at),
        "updatedAt" => Out::Leaf(iso_or(&i.updated_at, &i.created_at)),
        "lastEditedAt" | "activeLockReason" | "editor" => Out::Null,
        "author" => i.author.as_ref().map(actor).unwrap_or_else(ghost),
        "authorAssociation" => association(&i.author_association),
        "locked" => Out::leaf(i.locked),
        "isPinned" => Out::leaf(i.pinned),
        "viewerCanUpdate" => Out::leaf(true),
        "viewerDidAuthor" => Out::leaf(i.author.as_ref().is_some_and(|a| a.id == me)),
        "viewerSubscription" => Out::leaf("UNSUBSCRIBED"),
        "repository" => Out::Obj(Node::Repo(r.clone())),
        "milestone" => i
            .milestone
            .as_ref()
            .map(|m| milestone_out(cx, repo, m))
            .unwrap_or(Out::Null),
        "reactionGroups" => reaction_groups(&i.reactions),
        "comments" | "timelineItems" => {
            let sub = Subject::Issue(Box::new((**i).clone()));
            let list = crate::rest::issues::comments_of(cx, repo, &sub).await?;
            let parent = Out::Obj(Node::Issue(r.clone(), i.clone()));
            let items = list
                .iter()
                .filter(|c| !c.deleted)
                .map(|c| comment_out(ex, repo, parent.clone(), false, i.number, c))
                .collect();
            let ty = if name == "comments" {
                "IssueCommentConnection"
            } else {
                "IssueTimelineItemsConnection"
            };
            connection(ty, items, args, Vec::new())
        }
        "participants" => {
            let mut ps: Vec<Principal> = i.author.iter().cloned().collect();
            ps.extend(i.assignees.iter().cloned());
            ps.dedup_by(|a, b| a.id == b.id);
            connection(
                "UserConnection",
                ps.iter().map(actor).collect(),
                args,
                Vec::new(),
            )
        }
        _ => Out::Null,
    })
}

/// GraphQL's land evaluation for a PR (mergeability and policy).
async fn evaluation(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> LandingEvaluation {
    match cx.hub.changes.as_ref() {
        Some(svc) => svc
            .evaluate_landing(cx.req(EvaluateLandingRequest {
                repo: rid(&repo.id),
                change_request_id: c.id.clone(),
            }))
            .await
            .map(|r| r.into_inner())
            .unwrap_or(LandingEvaluation {
                mergeable: true,
                ..Default::default()
            }),
        None => LandingEvaluation {
            mergeable: true,
            ..Default::default()
        },
    }
}

async fn reviews_of(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> Vec<Review> {
    match cx.hub.changes.as_ref() {
        Some(svc) => svc
            .list_reviews(cx.req(ListReviewsRequest {
                repo: rid(&repo.id),
                change_request_id: c.id.clone(),
            }))
            .await
            .map(|r| r.into_inner().reviews)
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

fn review_state(r: &Review) -> &'static str {
    if r.dismissed {
        return "DISMISSED";
    }
    match Verdict::try_from(r.verdict).unwrap_or_default() {
        Verdict::Approve => "APPROVED",
        Verdict::RequestChanges => "CHANGES_REQUESTED",
        _ => "COMMENTED",
    }
}

async fn review_out(cx: &Ctx, r: &Arc<Repo>, c: &Arc<ChangeRequest>, rv: &Review) -> Out {
    let id = cx
        .hub
        .ids
        .remember("review", &rv.id, &format!("{}:{}", r.id, c.number));
    let path = format!("/{}/pull/{}#pullrequestreview-{id}", r.full_name, c.number);
    let commit = match c
        .revisions
        .iter()
        .find(|x| x.n == rv.revision)
        .and_then(|x| x.head.as_ref())
    {
        Some(h) => match cx.hub.ports.git.git_id(r, h).await.ok().flatten() {
            Some(oid) => Out::map(
                "Commit",
                vec![
                    ("id", Out::leaf(node("Commit", &format!("{}:{oid}", r.id)))),
                    (
                        "abbreviatedOid",
                        Out::leaf(oid.chars().take(7).collect::<String>()),
                    ),
                    ("oid", Out::leaf(oid)),
                ],
            ),
            None => Out::Null,
        },
        None => Out::Null,
    };
    Out::map(
        "PullRequestReview",
        vec![
            ("id", Out::leaf(node("PullRequestReview", &rv.id))),
            ("databaseId", Out::leaf(id)),
            (
                "author",
                rv.reviewer.as_ref().map(actor).unwrap_or_else(ghost),
            ),
            ("authorAssociation", Out::leaf("MEMBER")),
            ("body", Out::leaf(rv.body.clone())),
            ("bodyText", Out::leaf(rv.body.clone())),
            ("bodyHTML", Out::leaf(rv.body.clone())),
            ("state", Out::leaf(review_state(rv))),
            ("submittedAt", opt_ts(&rv.created_at)),
            ("createdAt", opt_ts(&rv.created_at)),
            ("updatedAt", opt_ts(&rv.created_at)),
            ("includesCreatedEdit", Out::leaf(false)),
            ("url", Out::leaf(cx.html(&path))),
            ("resourcePath", Out::leaf(path)),
            ("commit", commit),
            ("reactionGroups", reaction_groups(&[])),
            ("pullRequest", Out::Obj(Node::Pull(r.clone(), c.clone()))),
            (
                "viewerDidAuthor",
                Out::leaf(rv.reviewer.as_ref().is_some_and(|x| x.id == cx.me())),
            ),
        ],
    )
}

/// The latest review per reviewer, in submission order.
fn latest_reviews(reviews: &[Review], opinionated: bool) -> Vec<Review> {
    let mut by: BTreeMap<String, Review> = BTreeMap::new();
    for rv in reviews {
        let who = rv
            .reviewer
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        if opinionated && review_state(rv) == "COMMENTED" {
            continue;
        }
        by.insert(who, rv.clone());
    }
    let mut out: Vec<Review> = by.into_values().collect();
    out.sort_by(|a, b| {
        a.created_at
            .as_ref()
            .map(|t| (t.seconds, t.nanos))
            .cmp(&b.created_at.as_ref().map(|t| (t.seconds, t.nanos)))
    });
    out
}

async fn pull_field(
    ex: &Exec<'_>,
    r: &Arc<Repo>,
    c: &Arc<ChangeRequest>,
    name: &str,
    args: &Args,
) -> GResult {
    let cx = &ex.cx;
    let repo: &Repo = r;
    if let Some(o) = labels_assignees_common(ex, repo, name, args, &c.labels, &c.assignees).await {
        return Ok(o);
    }
    let (state, merged) = shapes::cr_state(c);
    let path = format!("/{}/pull/{}", repo.full_name, c.number);
    let me = cx.me();
    let self_out = || Out::Obj(Node::Pull(r.clone(), c.clone()));
    Ok(match name {
        "id" => Out::leaf(node("PullRequest", &format!("{}:{}", repo.id, c.number))),
        "databaseId" => Out::leaf(cx.hub.ids.remember(
            "pull",
            &c.id,
            &format!("{}:{}", repo.id, c.number),
        )),
        "fullDatabaseId" => Out::leaf(num("pull", &c.id).to_string()),
        "number" => Out::leaf(c.number),
        "title" | "titleHTML" => Out::leaf(c.title.clone()),
        "body" | "bodyText" | "bodyHTML" => Out::leaf(c.body.clone()),
        "url" | "permalink" => Out::leaf(cx.html(&path)),
        "resourcePath" => Out::leaf(path),
        "state" => Out::leaf(match (state, merged) {
            (_, true) => "MERGED",
            ("open", _) => "OPEN",
            _ => "CLOSED",
        }),
        "closed" => Out::leaf(state == "closed"),
        "merged" => Out::leaf(merged),
        "isDraft" => Out::leaf(c.draft),
        "locked" => Out::leaf(c.locked),
        "activeLockReason"
        | "lastEditedAt"
        | "editor"
        | "mergedBy"
        | "mergeQueueEntry"
        | "potentialMergeCommit" => Out::Null,
        "createdAt" => opt_ts(&c.created_at),
        "updatedAt" => Out::Leaf(iso_or(&c.updated_at, &c.created_at)),
        "closedAt" => Out::Leaf(iso_or(&c.closed_at, &c.landed_at)),
        "mergedAt" => {
            if merged {
                Out::Leaf(iso_or(&c.landed_at, &c.closed_at))
            } else {
                Out::Null
            }
        }
        "author" => c.author.as_ref().map(actor).unwrap_or_else(ghost),
        "authorAssociation" => Out::leaf("MEMBER"),
        "headRefName" => Out::leaf(pulls::head_ref(cx, repo, c).await),
        "headRefOid" => Out::leaf(pulls::shas(cx, repo, c).await.0),
        "baseRefName" => Out::leaf(c.target_ref.clone()),
        "baseRefOid" => Out::leaf(pulls::shas(cx, repo, c).await.1),
        "headRef" => {
            let h = pulls::head_ref(cx, repo, c).await;
            let head_repo = pull_head_repo(cx, repo, c).await;
            match cx.hub.ports.git.resolve(&head_repo, &h).await? {
                Some(sha) => ref_out(cx, &head_repo, &format!("refs/heads/{h}"), &sha).await,
                None => Out::Null,
            }
        }
        "headRepository" if is_cross(repo, c) => {
            Out::Obj(Node::Repo(Arc::new(pull_head_repo(cx, repo, c).await)))
        }
        "headRepositoryOwner" if is_cross(repo, c) => {
            let f = pull_head_repo(cx, repo, c).await;
            let (owner, _) = shapes::split(&f);
            org_out(cx, owner).await.unwrap_or(Out::Null)
        }
        "isCrossRepository" => Out::leaf(is_cross(repo, c)),
        "maintainerCanModify" => Out::leaf(c.allow_maintainer_edits),
        "baseRef" => match cx.hub.ports.git.resolve(repo, &c.target_ref).await? {
            Some(sha) => ref_out(cx, repo, &format!("refs/heads/{}", c.target_ref), &sha).await,
            None => Out::Null,
        },
        "headRepository" | "baseRepository" | "repository" => Out::Obj(Node::Repo(r.clone())),
        "headRepositoryOwner" => {
            let (owner, _) = shapes::split(repo);
            org_out(cx, owner).await.unwrap_or(Out::Null)
        }
        "isInMergeQueue" | "viewerCanMergeAsAdmin" => Out::leaf(false),
        "isMergeQueueEnabled" => Out::leaf(
            crate::rest::settings(cx, repo)
                .await
                .is_some_and(|s| s.merge_queue_required),
        ),
        "viewerCanUpdate" | "viewerCanEditFiles" => Out::leaf(true),
        "viewerDidAuthor" => Out::leaf(c.author.as_ref().is_some_and(|a| a.id == me)),
        "viewerSubscription" => Out::leaf("UNSUBSCRIBED"),
        "viewerMergeHeadlineText" => Out::leaf(format!(
            "Merge pull request #{} from {}",
            c.number,
            pulls::head_ref(cx, repo, c).await
        )),
        "viewerMergeBodyText" => Out::leaf(c.title.clone()),
        "mergeable" | "canBeRebased" => {
            let e = evaluation(cx, repo, c).await;
            if name == "canBeRebased" {
                Out::leaf(e.mergeable)
            } else if state != "open" {
                Out::leaf("UNKNOWN")
            } else {
                Out::leaf(if e.mergeable {
                    "MERGEABLE"
                } else {
                    "CONFLICTING"
                })
            }
        }
        "mergeStateStatus" => {
            let e = evaluation(cx, repo, c).await;
            Out::leaf(if state != "open" {
                "UNKNOWN"
            } else if !e.mergeable {
                "DIRTY"
            } else if c.draft {
                "DRAFT"
            } else if e.allowed {
                "CLEAN"
            } else {
                "BLOCKED"
            })
        }
        "reviewDecision" => {
            let reviews = reviews_of(cx, repo, c).await;
            let latest = latest_reviews(&reviews, true);
            if latest
                .iter()
                .any(|x| review_state(x) == "CHANGES_REQUESTED")
            {
                Out::leaf("CHANGES_REQUESTED")
            } else if latest.iter().any(|x| review_state(x) == "APPROVED") {
                Out::leaf("APPROVED")
            } else {
                let e = evaluation(cx, repo, c).await;
                if e.missing
                    .iter()
                    .any(|m| m.to_lowercase().contains("approv"))
                {
                    Out::leaf("REVIEW_REQUIRED")
                } else {
                    Out::Null
                }
            }
        }
        "additions"
        | "deletions"
        | "changedFiles"
        | "files"
        | "commits"
        | "timelineItems_commits" => {
            let (head, base) = pulls::shas(cx, repo, c).await;
            let cmp = if head.is_empty() || base.is_empty() {
                crate::ports::Comparison::default()
            } else {
                cx.hub
                    .ports
                    .git
                    .compare(repo, &base, &head)
                    .await
                    .unwrap_or_default()
            };
            match name {
                "additions" => Out::leaf(cmp.files.iter().map(|f| f.additions).sum::<u64>()),
                "deletions" => Out::leaf(cmp.files.iter().map(|f| f.deletions).sum::<u64>()),
                "changedFiles" => Out::leaf(cmp.files.len()),
                "files" => {
                    let items = cmp
                        .files
                        .iter()
                        .map(|f| {
                            Out::map(
                                "PullRequestChangedFile",
                                vec![
                                    ("path", Out::leaf(f.filename.clone())),
                                    ("additions", Out::leaf(f.additions)),
                                    ("deletions", Out::leaf(f.deletions)),
                                    (
                                        "changeType",
                                        Out::leaf(match f.status.as_str() {
                                            "added" => "ADDED",
                                            "removed" => "DELETED",
                                            "renamed" => "RENAMED",
                                            "changed" => "CHANGED",
                                            _ => "MODIFIED",
                                        }),
                                    ),
                                    ("viewerViewedState", Out::leaf("UNVIEWED")),
                                ],
                            )
                        })
                        .collect();
                    connection("PullRequestChangedFileConnection", items, args, Vec::new())
                }
                _ => {
                    let items = cmp
                        .ahead
                        .iter()
                        .map(|gc| {
                            Out::map(
                                "PullRequestCommit",
                                vec![
                                    (
                                        "id",
                                        Out::leaf(node(
                                            "PullRequestCommit",
                                            &format!("{}:{}:{}", repo.id, c.number, gc.sha),
                                        )),
                                    ),
                                    (
                                        "commit",
                                        Out::Obj(Node::Commit(
                                            r.clone(),
                                            Arc::new(gc.clone()),
                                            Some(c.target_ref.clone()),
                                        )),
                                    ),
                                    (
                                        "url",
                                        Out::leaf(cx.html(&format!(
                                            "/{}/pull/{}/commits/{}",
                                            repo.full_name, c.number, gc.sha
                                        ))),
                                    ),
                                    (
                                        "resourcePath",
                                        Out::leaf(format!(
                                            "/{}/pull/{}/commits/{}",
                                            repo.full_name, c.number, gc.sha
                                        )),
                                    ),
                                    ("pullRequest", self_out()),
                                ],
                            )
                        })
                        .collect();
                    connection("PullRequestCommitConnection", items, args, Vec::new())
                }
            }
        }
        "statusCheckRollup" => {
            let (head, _) = pulls::shas(cx, repo, c).await;
            rollup_or_null(cx, r, &head, Some(&c.target_ref)).await?
        }
        "mergeCommit" => match c.landed_ref.as_ref().and_then(|x| x.target.as_ref()) {
            Some(t) if merged => match cx.hub.ports.git.git_id(repo, t).await? {
                Some(oid) => match cx.hub.ports.git.commit(repo, &oid).await? {
                    Some(gc) => Out::Obj(Node::Commit(r.clone(), Arc::new(gc), None)),
                    None => Out::Null,
                },
                None => Out::Null,
            },
            _ => Out::Null,
        },
        "autoMergeRequest" => match &c.auto_merge {
            Some(a) => Out::map(
                "AutoMergeRequest",
                vec![
                    ("enabledBy", actor_opt(&a.enabled_by)),
                    ("enabledAt", opt_ts(&a.enabled_at)),
                    (
                        "mergeMethod",
                        Out::leaf(match LandMethod::try_from(a.method).unwrap_or_default() {
                            LandMethod::Squash => "SQUASH",
                            LandMethod::MergeCommit => "MERGE",
                            _ => "REBASE",
                        }),
                    ),
                    (
                        "commitHeadline",
                        Out::opt_str(a.commit_message.lines().next().unwrap_or("")),
                    ),
                    (
                        "commitBody",
                        Out::opt_str(
                            a.commit_message
                                .split_once("\n\n")
                                .map(|x| x.1)
                                .unwrap_or(""),
                        ),
                    ),
                    ("authorEmail", Out::Null),
                    ("pullRequest", self_out()),
                ],
            ),
            None => Out::Null,
        },
        "milestone" => c
            .milestone
            .as_ref()
            .map(|m| milestone_out(cx, repo, m))
            .unwrap_or(Out::Null),
        "reactionGroups" => reaction_groups(&c.reactions),
        "comments" | "timelineItems" => {
            let sub = Subject::Pull(Box::new((**c).clone()));
            let list = crate::rest::issues::comments_of(cx, repo, &sub).await?;
            let items = list
                .iter()
                .filter(|x| !x.deleted)
                .map(|x| comment_out(ex, repo, self_out(), true, c.number, x))
                .collect();
            let ty = if name == "comments" {
                "IssueCommentConnection"
            } else {
                "IssueTimelineItemsConnection"
            };
            connection(ty, items, args, Vec::new())
        }
        "reviews" | "latestReviews" | "latestOpinionatedReviews" => {
            let all = reviews_of(cx, repo, c).await;
            let mut list = match name {
                "reviews" => all,
                "latestReviews" => latest_reviews(&all, false),
                _ => latest_reviews(&all, true),
            };
            let states = a_list(args, "states");
            if !states.is_empty() {
                list.retain(|x| states.iter().any(|s| s == review_state(x)));
            }
            let mut items = Vec::new();
            for rv in &list {
                items.push(review_out(cx, r, c, rv).await);
            }
            connection("PullRequestReviewConnection", items, args, Vec::new())
        }
        "reviewRequests" => {
            let (owner, _) = shapes::split(repo);
            let mut items = Vec::new();
            for rr in &c.review_requests {
                let who = match &rr.reviewer {
                    Some(p) if !p.id.is_empty() => actor(p),
                    _ => {
                        let t = crate::rest::orgs::team_ref(cx, owner, &rr.team_id).await;
                        Out::map(
                            "Team",
                            vec![
                                ("id", Out::leaf(node("Team", &rr.team_id))),
                                ("name", Out::Leaf(t["name"].clone())),
                                ("slug", Out::Leaf(t["slug"].clone())),
                                (
                                    "combinedSlug",
                                    Out::leaf(format!(
                                        "{owner}/{}",
                                        t["slug"].as_str().unwrap_or("")
                                    )),
                                ),
                                (
                                    "organization",
                                    org_out(cx, owner).await.unwrap_or(Out::Null),
                                ),
                            ],
                        )
                    }
                };
                items.push(Out::map(
                    "ReviewRequest",
                    vec![
                        (
                            "id",
                            Out::leaf(node(
                                "ReviewRequest",
                                &format!(
                                    "{}:{}",
                                    c.id,
                                    rr.reviewer
                                        .as_ref()
                                        .map(|p| p.id.as_str())
                                        .unwrap_or(&rr.team_id)
                                ),
                            )),
                        ),
                        ("asCodeOwner", Out::leaf(rr.auto_assigned)),
                        ("requestedReviewer", who),
                        ("pullRequest", self_out()),
                    ],
                ));
            }
            connection("ReviewRequestConnection", items, args, Vec::new())
        }
        "reviewThreads" => {
            let threads = pulls::threads(cx, repo, c).await.unwrap_or_default();
            let items = threads
                .iter()
                .map(|t| {
                    let comments: Vec<Out> = t
                        .comments
                        .iter()
                        .map(|rc| {
                            Out::map(
                                "PullRequestReviewComment",
                                vec![
                                    ("id", Out::leaf(node("PullRequestReviewComment", &rc.id))),
                                    ("databaseId", Out::leaf(num("review_comment", &rc.id))),
                                    (
                                        "author",
                                        rc.author.as_ref().map(actor).unwrap_or_else(ghost),
                                    ),
                                    ("authorAssociation", Out::leaf("MEMBER")),
                                    ("body", Out::leaf(rc.body.clone())),
                                    ("bodyText", Out::leaf(rc.body.clone())),
                                    ("createdAt", opt_ts(&rc.created_at)),
                                    (
                                        "path",
                                        Out::leaf(String::from_utf8_lossy(&rc.path).into_owned()),
                                    ),
                                    (
                                        "line",
                                        if rc.file_level {
                                            Out::Null
                                        } else {
                                            Out::leaf(rc.end_line)
                                        },
                                    ),
                                    (
                                        "url",
                                        Out::leaf(cx.html(&format!(
                                            "/{}/pull/{}#discussion_r{}",
                                            repo.full_name,
                                            c.number,
                                            num("review_comment", &rc.id)
                                        ))),
                                    ),
                                ],
                            )
                        })
                        .collect();
                    Out::map(
                        "PullRequestReviewThread",
                        vec![
                            ("id", Out::leaf(node("PullRequestReviewThread", &t.id))),
                            (
                                "path",
                                Out::leaf(String::from_utf8_lossy(&t.path).into_owned()),
                            ),
                            ("line", Out::leaf(t.end_line)),
                            (
                                "startLine",
                                if t.start_line == t.end_line {
                                    Out::Null
                                } else {
                                    Out::leaf(t.start_line)
                                },
                            ),
                            (
                                "diffSide",
                                Out::leaf(if t.side == "old" { "LEFT" } else { "RIGHT" }),
                            ),
                            ("isResolved", Out::leaf(t.resolved)),
                            ("isOutdated", Out::leaf(t.outdated)),
                            ("resolvedBy", actor_opt(&t.resolved_by)),
                            (
                                "comments",
                                connection(
                                    "PullRequestReviewCommentConnection",
                                    comments,
                                    &Map::new(),
                                    Vec::new(),
                                ),
                            ),
                        ],
                    )
                })
                .collect();
            connection("PullRequestReviewThreadConnection", items, args, Vec::new())
        }
        "participants" => {
            let mut ps: Vec<Principal> = c.author.iter().cloned().collect();
            ps.extend(c.assignees.iter().cloned());
            ps.dedup_by(|a, b| a.id == b.id);
            connection(
                "UserConnection",
                ps.iter().map(actor).collect(),
                args,
                Vec::new(),
            )
        }
        "closingIssuesReferences" => {
            let mut items = Vec::new();
            for n in &c.linked_issues {
                if let Ok(Subject::Issue(i)) = crate::rest::issues::subject(cx, repo, *n).await {
                    items.push(Out::Obj(Node::Issue(r.clone(), Arc::new(*i))));
                }
            }
            connection("IssueConnection", items, args, Vec::new())
        }
        _ => Out::Null,
    })
}

// ----- commits and checks -------------------------------------------------------------------

fn git_actor(cx: &Ctx, s: &crate::ports::Signature) -> Out {
    Out::map(
        "GitActor",
        vec![
            ("name", Out::leaf(s.name.clone())),
            ("email", Out::leaf(s.email.clone())),
            ("date", Out::leaf(iso_secs(s.time_secs))),
            ("avatarUrl", Out::leaf(cx.html(&format!("/{}.png", s.name)))),
            ("user", Out::Null),
        ],
    )
}

async fn git_actor_user(cx: &Ctx, s: &crate::ports::Signature) -> Out {
    let a = git_actor(cx, s);
    let user = match cx.by_login(&s.name).await {
        Ok(p) => actor(&p),
        Err(_) => Out::Null,
    };
    match a {
        Out::Obj(Node::Map(t, m)) => {
            let mut m = (*m).clone();
            m.insert("user".into(), user);
            Out::Obj(Node::Map(t, Arc::new(m)))
        }
        other => other,
    }
}

async fn commit_field(
    ex: &Exec<'_>,
    r: &Arc<Repo>,
    c: &Arc<GitCommitInfo>,
    target: Option<&str>,
    name: &str,
    args: &Args,
) -> GResult {
    let cx = &ex.cx;
    let repo: &Repo = r;
    let (headline, body) = match c.message.split_once('\n') {
        Some((h, b)) => (h.trim_end().to_string(), b.trim().to_string()),
        None => (c.message.trim_end().to_string(), String::new()),
    };
    let path = format!("/{}/commit/{}", repo.full_name, c.sha);
    Ok(match name {
        "id" => Out::leaf(node("Commit", &format!("{}:{}", repo.id, c.sha))),
        "oid" => Out::leaf(c.sha.clone()),
        "abbreviatedOid" => Out::leaf(c.sha.chars().take(7).collect::<String>()),
        "commitUrl" | "url" => Out::leaf(cx.html(&path)),
        "resourcePath" => Out::leaf(path),
        "repository" => Out::Obj(Node::Repo(r.clone())),
        "message" => Out::leaf(c.message.clone()),
        "messageHeadline" | "messageHeadlineHTML" => Out::leaf(headline),
        "messageBody" | "messageBodyHTML" => Out::leaf(body),
        "committedDate" => Out::leaf(iso_secs(c.committer.time_secs)),
        "authoredDate" => Out::leaf(iso_secs(c.author.time_secs)),
        "pushedDate" => Out::Null,
        "additions" | "deletions" | "changedFilesIfAvailable" => {
            let files = match c.parents.first() {
                Some(p) => cx
                    .hub
                    .ports
                    .git
                    .diff(repo, p, &c.sha)
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            match name {
                "additions" => Out::leaf(files.iter().map(|f| f.additions).sum::<u64>()),
                "deletions" => Out::leaf(files.iter().map(|f| f.deletions).sum::<u64>()),
                _ => Out::leaf(files.len()),
            }
        }
        "author" => git_actor_user(cx, &c.author).await,
        "committer" => git_actor_user(cx, &c.committer).await,
        "authors" => connection(
            "GitActorConnection",
            vec![git_actor_user(cx, &c.author).await],
            args,
            Vec::new(),
        ),
        "parents" => {
            let mut items = Vec::new();
            for p in &c.parents {
                if let Some(pc) = cx.hub.ports.git.commit(repo, p).await? {
                    items.push(Out::Obj(Node::Commit(r.clone(), Arc::new(pc), None)));
                }
            }
            connection("CommitConnection", items, args, Vec::new())
        }
        "tree" => Out::map(
            "Tree",
            vec![
                (
                    "id",
                    Out::leaf(node("Tree", &format!("{}:{}", repo.id, c.tree))),
                ),
                ("oid", Out::leaf(c.tree.clone())),
                (
                    "abbreviatedOid",
                    Out::leaf(c.tree.chars().take(7).collect::<String>()),
                ),
                (
                    "commitUrl",
                    Out::leaf(cx.html(&format!("/{}/commit/{}", repo.full_name, c.sha))),
                ),
                ("repository", Out::Obj(Node::Repo(r.clone()))),
                ("entries", Out::Null),
            ],
        ),
        "statusCheckRollup" => rollup_or_null(cx, r, &c.sha, target).await?,
        "status" => {
            let Some(cs) = cx.hub.ports.git.changeset(repo, &c.sha).await? else {
                return Ok(Out::Null);
            };
            let list = crate::rest::checks::statuses_of(cx, repo, &cs).await?;
            if list.is_empty() {
                return Ok(Out::Null);
            }
            let (state, latest) = crate::rest::checks::combine(&list);
            let contexts = latest.iter().map(|s| status_context_out(s, &[])).collect();
            Out::map(
                "Status",
                vec![
                    (
                        "id",
                        Out::leaf(node("Status", &format!("{}:{}", repo.id, c.sha))),
                    ),
                    (
                        "state",
                        Out::leaf(crate::rest::checks::state_word(state).to_uppercase()),
                    ),
                    ("contexts", Out::List(contexts)),
                    ("context", Out::Null),
                    ("commit", Out::Obj(Node::Commit(r.clone(), c.clone(), None))),
                ],
            )
        }
        "checkSuites" => empty_connection("CheckSuiteConnection", args),
        "associatedPullRequests" => {
            let mut items = Vec::new();
            if cx.hub.changes.is_some() {
                for cr in pulls::list_crs(cx, repo, "all", None, "").await? {
                    if pulls::shas(cx, repo, &cr).await.0 == c.sha {
                        items.push(Out::Obj(Node::Pull(r.clone(), Arc::new(cr))));
                    }
                }
            }
            connection("PullRequestConnection", items, args, Vec::new())
        }
        "history" => {
            let list = cx
                .hub
                .ports
                .git
                .log(repo, &c.sha, a_str(args, "path"), 0, crate::http::LIST_CAP)
                .await?;
            let items = list
                .into_iter()
                .map(|gc| Out::Obj(Node::Commit(r.clone(), Arc::new(gc), None)))
                .collect();
            connection("CommitHistoryConnection", items, args, Vec::new())
        }
        _ => Out::Null,
    })
}

/// Checks required by the land rule of `target`.
async fn required_checks(cx: &Ctx, repo: &Repo, target: Option<&str>) -> Vec<String> {
    let Some(t) = target else {
        return Vec::new();
    };
    let Some(src) = &cx.hub.forge.hooks.policy else {
        return Vec::new();
    };
    let Ok(Some(policy)) = src.policy(repo).await else {
        return Vec::new();
    };
    policy
        .doc()
        .land
        .iter()
        .filter(|(p, _)| p.matches(t))
        .max_by_key(|(p, _)| match p {
            bee_policy::RefPattern::Exact(_) => usize::MAX,
            bee_policy::RefPattern::Prefix(x) => x.len(),
        })
        .map(|(_, rule)| rule.body.required_checks.iter().cloned().collect())
        .unwrap_or_default()
}

fn status_context_out(s: &bee_proto::v1::CommitStatus, required: &[String]) -> Out {
    Out::map(
        "StatusContext",
        vec![
            ("id", Out::leaf(node("StatusContext", &s.id))),
            ("context", Out::leaf(s.context.clone())),
            (
                "state",
                Out::leaf(
                    crate::rest::checks::state_word(
                        CommitStatusState::try_from(s.state).unwrap_or_default(),
                    )
                    .to_uppercase(),
                ),
            ),
            ("targetUrl", Out::opt_str(&s.target_url)),
            ("description", Out::opt_str(&s.description)),
            ("createdAt", opt_ts(&s.created_at)),
            ("avatarUrl", Out::Null),
            ("creator", actor_opt(&s.creator)),
            ("isRequired", Out::leaf(required.contains(&s.context))),
            ("commit", Out::Null),
        ],
    )
}

async fn check_suite_out(cx: &Ctx, repo: &Repo, run: &CheckRun) -> Out {
    let (st, concl) =
        crate::rest::checks::check_words(CheckStatus::try_from(run.status).unwrap_or_default());
    let mut workflow_run = Out::Null;
    if !run.workflow_job_id.is_empty()
        && let Some(ci) = &cx.hub.ci
        && let Ok(job) = ci
            .get_workflow_job(cx.req(GetWorkflowJobRequest {
                repo: rid(&repo.id),
                job_id: run.workflow_job_id.clone(),
            }))
            .await
        && let Ok(wr) = ci
            .get_workflow_run(cx.req(GetWorkflowRunRequest {
                repo: rid(&repo.id),
                run_id: job.into_inner().run_id,
            }))
            .await
    {
        let wr = wr.into_inner();
        let id = cx.hub.ids.remember("run", &wr.id, &repo.id);
        workflow_run = Out::map(
            "WorkflowRun",
            vec![
                ("id", Out::leaf(node("WorkflowRun", &wr.id))),
                ("databaseId", Out::leaf(id)),
                ("runNumber", Out::leaf(wr.run_number)),
                ("event", Out::leaf(wr.event.clone())),
                (
                    "url",
                    Out::leaf(cx.html(&format!("/{}/actions/runs/{id}", repo.full_name))),
                ),
                ("createdAt", opt_ts(&wr.created_at)),
                (
                    "updatedAt",
                    Out::Leaf(iso_or(&wr.updated_at, &wr.created_at)),
                ),
                (
                    "workflow",
                    Out::map(
                        "Workflow",
                        vec![
                            ("id", Out::leaf(node("Workflow", &wr.workflow_id))),
                            ("databaseId", Out::leaf(num("workflow", &wr.workflow_id))),
                            ("name", Out::leaf(wr.name.clone())),
                            (
                                "url",
                                Out::leaf(cx.html(&format!("/{}/actions", repo.full_name))),
                            ),
                            (
                                "resourcePath",
                                Out::leaf(format!("/{}/actions", repo.full_name)),
                            ),
                            ("createdAt", opt_ts(&wr.created_at)),
                            ("updatedAt", opt_ts(&wr.updated_at)),
                        ],
                    ),
                ),
            ],
        );
    }
    let reporter = run.reporter.clone().unwrap_or_default();
    let login = cx.login(&reporter).await;
    Out::map(
        "CheckSuite",
        vec![
            (
                "id",
                Out::leaf(node(
                    "CheckSuite",
                    if run.suite_id.is_empty() {
                        &run.id
                    } else {
                        &run.suite_id
                    },
                )),
            ),
            ("databaseId", Out::leaf(num("check_suite", &run.suite_id))),
            ("status", Out::leaf(st.to_uppercase())),
            (
                "conclusion",
                concl
                    .map(|c| Out::leaf(c.to_uppercase()))
                    .unwrap_or(Out::Null),
            ),
            (
                "app",
                Out::map(
                    "App",
                    vec![
                        ("id", Out::leaf(node("App", &reporter.id))),
                        ("databaseId", Out::leaf(num("app", &reporter.id))),
                        (
                            "name",
                            Out::leaf(if reporter.display.is_empty() {
                                login.clone()
                            } else {
                                reporter.display.clone()
                            }),
                        ),
                        ("slug", Out::leaf(login.clone())),
                        ("url", Out::leaf(cx.html(&format!("/apps/{login}")))),
                        ("logoUrl", Out::leaf(cx.html(&format!("/{login}.png")))),
                    ],
                ),
            ),
            ("workflowRun", workflow_run),
            ("commit", Out::Null),
            ("branch", Out::Null),
            ("createdAt", opt_ts(&run.started_at)),
            (
                "updatedAt",
                Out::Leaf(iso_or(&run.finished_at, &run.started_at)),
            ),
        ],
    )
}

async fn check_run_out(cx: &Ctx, repo: &Repo, run: &CheckRun, required: &[String]) -> Out {
    let (st, concl) =
        crate::rest::checks::check_words(CheckStatus::try_from(run.status).unwrap_or_default());
    let id = cx.hub.ids.remember("check_run", &run.id, &repo.id);
    let out = run.output.clone().unwrap_or_default();
    Out::map(
        "CheckRun",
        vec![
            ("id", Out::leaf(node("CheckRun", &run.id))),
            ("databaseId", Out::leaf(id)),
            ("name", Out::leaf(run.name.clone())),
            ("status", Out::leaf(st.to_uppercase())),
            (
                "conclusion",
                concl
                    .map(|c| Out::leaf(c.to_uppercase()))
                    .unwrap_or(Out::Null),
            ),
            ("startedAt", opt_ts(&run.started_at)),
            ("completedAt", opt_ts(&run.finished_at)),
            ("detailsUrl", Out::opt_str(&run.url)),
            ("externalId", Out::opt_str(&run.external_id)),
            (
                "summary",
                Out::opt_str(if out.summary.is_empty() {
                    &run.summary
                } else {
                    &out.summary
                }),
            ),
            ("text", Out::opt_str(&out.text)),
            ("title", Out::opt_str(&out.title)),
            (
                "url",
                Out::leaf(cx.html(&format!("/{}/runs/{id}", repo.full_name))),
            ),
            (
                "resourcePath",
                Out::leaf(format!("/{}/runs/{id}", repo.full_name)),
            ),
            (
                "permalink",
                Out::leaf(cx.html(&format!("/{}/runs/{id}", repo.full_name))),
            ),
            ("isRequired", Out::leaf(required.contains(&run.name))),
            ("checkSuite", check_suite_out(cx, repo, run).await),
        ],
    )
}

/// The contexts of a commit: latest check run per name, then latest status per context.
async fn contexts(
    cx: &Ctx,
    repo: &Repo,
    sha: &str,
    target: Option<&str>,
) -> Result<(Vec<Out>, Vec<CheckRun>, Vec<bee_proto::v1::CommitStatus>), GqlError> {
    let Some(cs) = cx.hub.ports.git.changeset(repo, sha).await? else {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    };
    let runs = cx
        .hub
        .svc
        .list_checks(cx.req(ListChecksRequest {
            repo: rid(&repo.id),
            changeset: Some(cs.clone()),
            name: String::new(),
            suite_id: String::new(),
            latest_only: true,
        }))
        .await?
        .into_inner()
        .checks
        .into_iter()
        .filter(|r| !crate::rest::checks::is_status_shim(r))
        .collect::<Vec<_>>();
    let statuses = crate::rest::checks::statuses_of(cx, repo, &cs).await?;
    let (_, latest) = crate::rest::checks::combine(&statuses);
    let required = required_checks(cx, repo, target).await;
    let mut items = Vec::new();
    for run in &runs {
        items.push(check_run_out(cx, repo, run, &required).await);
    }
    for s in &latest {
        items.push(status_context_out(s, &required));
    }
    Ok((items, runs, latest))
}

fn rollup_state(runs: &[CheckRun], statuses: &[bee_proto::v1::CommitStatus]) -> &'static str {
    let mut pending = false;
    for r in runs {
        match CheckStatus::try_from(r.status).unwrap_or_default() {
            CheckStatus::Failed
            | CheckStatus::TimedOut
            | CheckStatus::Cancelled
            | CheckStatus::ActionRequired
            | CheckStatus::Stale => {
                return "FAILURE";
            }
            CheckStatus::Queued | CheckStatus::Running | CheckStatus::Unspecified => pending = true,
            _ => {}
        }
    }
    for s in statuses {
        match CommitStatusState::try_from(s.state).unwrap_or_default() {
            CommitStatusState::Failure => return "FAILURE",
            CommitStatusState::Error => return "ERROR",
            CommitStatusState::Pending | CommitStatusState::Unspecified => pending = true,
            _ => {}
        }
    }
    if pending { "PENDING" } else { "SUCCESS" }
}

async fn rollup_or_null(cx: &Ctx, r: &Arc<Repo>, sha: &str, target: Option<&str>) -> GResult {
    if sha.is_empty() {
        return Ok(Out::Null);
    }
    let (items, _, _) = contexts(cx, r, sha, target).await?;
    if items.is_empty() {
        return Ok(Out::Null);
    }
    Ok(Out::Obj(Node::Rollup(
        r.clone(),
        sha.to_string(),
        target.map(str::to_string),
    )))
}

async fn rollup_field(
    ex: &Exec<'_>,
    r: &Arc<Repo>,
    sha: &str,
    target: Option<&str>,
    name: &str,
    args: &Args,
) -> GResult {
    let cx = &ex.cx;
    Ok(match name {
        "id" => Out::leaf(node("StatusCheckRollup", &format!("{}:{sha}", r.id))),
        "state" => {
            let (_, runs, statuses) = contexts(cx, r, sha, target).await?;
            Out::leaf(rollup_state(&runs, &statuses))
        }
        "commit" => match cx.hub.ports.git.commit(r, sha).await? {
            Some(c) => Out::Obj(Node::Commit(
                r.clone(),
                Arc::new(c),
                target.map(str::to_string),
            )),
            None => Out::Null,
        },
        "contexts" => {
            let (items, runs, statuses) = contexts(cx, r, sha, target).await?;
            let mut by_run: BTreeMap<&str, usize> = BTreeMap::new();
            for run in &runs {
                let (st, concl) = crate::rest::checks::check_words(
                    CheckStatus::try_from(run.status).unwrap_or_default(),
                );
                let key = concl.unwrap_or(st);
                *by_run.entry(key).or_default() += 1;
            }
            let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
            for s in &statuses {
                *by_status
                    .entry(
                        crate::rest::checks::state_word(
                            CommitStatusState::try_from(s.state).unwrap_or_default(),
                        )
                        .to_uppercase(),
                    )
                    .or_default() += 1;
            }
            let run_counts = by_run
                .into_iter()
                .map(|(k, v)| {
                    Out::map(
                        "CheckRunStateCount",
                        vec![
                            ("state", Out::leaf(k.to_uppercase())),
                            ("count", Out::leaf(v)),
                        ],
                    )
                })
                .collect();
            let status_counts = by_status
                .into_iter()
                .map(|(k, v)| {
                    Out::map(
                        "StatusContextStateCount",
                        vec![("state", Out::leaf(k)), ("count", Out::leaf(v))],
                    )
                })
                .collect();
            connection(
                "StatusCheckRollupContextConnection",
                items,
                args,
                vec![
                    ("checkRunCount", Out::leaf(runs.len())),
                    ("checkRunCountsByState", Out::List(run_counts)),
                    ("statusContextCount", Out::leaf(statuses.len())),
                    ("statusContextCountsByState", Out::List(status_counts)),
                ],
            )
        }
        _ => Out::Null,
    })
}

async fn release_field(
    ex: &Exec<'_>,
    r: &Arc<Repo>,
    rel: &Arc<Release>,
    name: &str,
    args: &Args,
) -> GResult {
    let cx = &ex.cx;
    let path = format!("/{}/releases/tag/{}", r.full_name, rel.tag_name);
    Ok(match name {
        "id" => Out::leaf(node("Release", &rel.id)),
        "databaseId" => Out::leaf(cx.hub.ids.remember("release", &rel.id, &r.id)),
        "name" => Out::opt_str(&rel.name),
        "tagName" => Out::leaf(rel.tag_name.clone()),
        "tagCommit" => match &rel.target {
            Some(t) => match cx.hub.ports.git.git_id(r, t).await? {
                Some(oid) => match cx.hub.ports.git.commit(r, &oid).await? {
                    Some(c) => Out::Obj(Node::Commit(r.clone(), Arc::new(c), None)),
                    None => Out::Null,
                },
                None => Out::Null,
            },
            None => Out::Null,
        },
        "description" => Out::opt_str(&rel.body),
        "isDraft" => Out::leaf(rel.draft),
        "isPrerelease" => Out::leaf(rel.prerelease),
        "isLatest" => Out::leaf(rel.latest),
        "isImmutable" => Out::leaf(rel.immutable),
        "createdAt" => opt_ts(&rel.created_at),
        "publishedAt" => opt_ts(&rel.published_at),
        "updatedAt" => Out::Leaf(iso_or(&rel.updated_at, &rel.created_at)),
        "url" => Out::leaf(cx.html(&path)),
        "resourcePath" => Out::leaf(path),
        "author" => actor_opt(&rel.author),
        "repository" => Out::Obj(Node::Repo(r.clone())),
        "releaseAssets" => {
            let want = a_str(args, "name");
            let items = rel
                .assets
                .iter()
                .filter(|a| want.is_none_or(|w| a.name == w))
                .map(|a| {
                    let id = cx.hub.ids.remember("asset", &a.id, &r.id);
                    Out::map(
                        "ReleaseAsset",
                        vec![
                            ("id", Out::leaf(node("ReleaseAsset", &a.id))),
                            ("name", Out::leaf(a.name.clone())),
                            ("contentType", Out::leaf(a.content_type.clone())),
                            ("size", Out::leaf(a.size)),
                            ("downloadCount", Out::leaf(a.download_count)),
                            (
                                "downloadUrl",
                                Out::leaf(cx.html(&format!(
                                    "/{}/releases/download/{}/{}",
                                    r.full_name, rel.tag_name, a.name
                                ))),
                            ),
                            (
                                "url",
                                Out::leaf(
                                    cx.api(&format!("/repos/{}/releases/assets/{id}", r.full_name)),
                                ),
                            ),
                            ("createdAt", opt_ts(&a.created_at)),
                            ("updatedAt", Out::Leaf(iso_or(&a.updated_at, &a.created_at))),
                        ],
                    )
                })
                .collect();
            connection("ReleaseAssetConnection", items, args, Vec::new())
        }
        _ => Out::Null,
    })
}

// ----- mutations ----------------------------------------------------------------------------

fn input(args: &Args) -> Map<String, Value> {
    args.get("input")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn client_id(i: &Map<String, Value>) -> Out {
    i.get("clientMutationId")
        .cloned()
        .map(Out::Leaf)
        .unwrap_or(Out::Null)
}

fn payload(i: &Map<String, Value>, ty: &str, mut fields: Vec<(&str, Out)>) -> Out {
    fields.push(("clientMutationId", client_id(i)));
    Out::map(ty, fields)
}

/// Principal ids from user node ids.
fn principal_ids(ids: &[String]) -> Result<Vec<String>, GqlError> {
    ids.iter()
        .map(|id| match parse_node(id) {
            Some(n) if n.kind == "User" || n.kind == "Bot" => Ok(n.key),
            _ => Err(not_found(id)),
        })
        .collect()
}

/// Label names from label node ids.
fn label_names(ids: &[String]) -> Result<Vec<String>, GqlError> {
    ids.iter()
        .map(|id| match parse_node(id) {
            Some(n) if n.kind == "Label" => Ok(n
                .key
                .split_once(':')
                .map(|x| x.1.to_string())
                .unwrap_or_default()),
            _ => Err(not_found(id)),
        })
        .collect()
}

fn milestone_number(id: &str) -> Result<u64, GqlError> {
    match parse_node(id) {
        Some(n) if n.kind == "Milestone" => n
            .key
            .rsplit(':')
            .next()
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| not_found(id)),
        _ => Err(not_found(id)),
    }
}

async fn logins(cx: &Ctx, ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for id in ids {
        out.push(cx.login(&cx.principal(id).await).await);
    }
    out
}

fn method_of(v: Option<&str>) -> LandMethod {
    match v {
        Some("SQUASH") => LandMethod::Squash,
        Some("REBASE") => LandMethod::Rebase,
        _ => LandMethod::MergeCommit,
    }
}

async fn refetch_pull(cx: &Ctx, repo: &Repo, number: u64) -> Out {
    match pulls::cr_by_number(cx, repo, number).await {
        Ok(Some(c)) => Out::Obj(Node::Pull(Arc::new(repo.clone()), Arc::new(c))),
        _ => Out::Null,
    }
}

async fn refetch_subject(cx: &Ctx, repo: &Repo, number: u64) -> Out {
    match crate::rest::issues::subject(cx, repo, number).await {
        Ok(s) => subject_out(repo, s),
        Err(_) => Out::Null,
    }
}

async fn mutation(ex: &Exec<'_>, name: &str, args: &Args) -> GResult {
    let cx = &ex.cx;
    let i = input(args);
    let s = |k: &str| i.get(k).and_then(Value::as_str).map(str::to_string);
    let list = |k: &str| -> Vec<String> {
        i.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let me = || {
        actor(&Principal {
            id: cx.me().to_string(),
            ..Default::default()
        })
    };
    match name {
        "createPullRequest" => {
            let rid_ = s("repositoryId").unwrap_or_default();
            let n = parse_node(&rid_)
                .filter(|n| n.kind == "Repository")
                .ok_or_else(|| not_found(&rid_))?;
            let repo = repo_by_id(cx, &n.key).await?;
            // `gh pr create` from a fork names the fork's node in `headRepositoryId`.
            let head_repo = match s("headRepositoryId")
                .and_then(|h| parse_node(&h))
                .filter(|n| n.kind == "Repository")
            {
                Some(h) => Some(repo_by_id(cx, &h.key).await?),
                None => None,
            };
            let c = pulls::open(
                cx,
                &repo,
                &s("headRefName").unwrap_or_default(),
                &s("baseRefName").unwrap_or_default(),
                &s("title").unwrap_or_default(),
                &s("body").unwrap_or_default(),
                i.get("draft").and_then(Value::as_bool).unwrap_or(false),
                head_repo,
                i.get("maintainerCanModify")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )
            .await?;
            Ok(payload(
                &i,
                "CreatePullRequestPayload",
                vec![(
                    "pullRequest",
                    Out::Obj(Node::Pull(Arc::new(repo), Arc::new(c))),
                )],
            ))
        }
        "updatePullRequest" => {
            let (repo, c) = pull_of(cx, &s("pullRequestId").unwrap_or_default()).await?;
            let mut body = json!({});
            if let Some(t) = s("title") {
                body["title"] = json!(t);
            }
            if let Some(b) = s("body") {
                body["body"] = json!(b);
            }
            if let Some(st) = s("state") {
                body["state"] = json!(st.to_lowercase());
            }
            if i.contains_key("labelIds") {
                body["labels"] = json!(label_names(&list("labelIds"))?);
            }
            if i.contains_key("assigneeIds") {
                body["assignees"] = json!(logins(cx, &principal_ids(&list("assigneeIds"))?).await);
            }
            if let Some(m) = i.get("milestoneId") {
                body["milestone"] = match m.as_str() {
                    Some(id) => json!(milestone_number(id)?),
                    None => Value::Null,
                };
            }
            let mut c = crate::rest::issues::update_pull(cx, &repo, c, &body).await?;
            if let Some(base) = s("baseRefName")
                && base != c.target_ref
            {
                c = crate::rest::changes(cx)?
                    .edit_change(cx.req(bee_proto::v1::EditChangeRequest {
                        repo: rid(&repo.id),
                        id: c.id.clone(),
                        target_ref: Some(base),
                        ..Default::default()
                    }))
                    .await?
                    .into_inner();
            }
            Ok(payload(
                &i,
                "UpdatePullRequestPayload",
                vec![
                    (
                        "pullRequest",
                        Out::Obj(Node::Pull(Arc::new(repo), Arc::new(c))),
                    ),
                    ("actor", me()),
                ],
            ))
        }
        "mergePullRequest" => {
            let (repo, c) = pull_of(cx, &s("pullRequestId").unwrap_or_default()).await?;
            let (cr, _) = pulls::land(
                cx,
                &repo,
                &c,
                method_of(s("mergeMethod").as_deref()),
                s("commitHeadline").as_deref(),
                s("commitBody").as_deref(),
                s("expectedHeadOid").as_deref(),
            )
            .await?;
            Ok(payload(
                &i,
                "MergePullRequestPayload",
                vec![
                    (
                        "pullRequest",
                        Out::Obj(Node::Pull(Arc::new(repo), Arc::new(cr))),
                    ),
                    ("actor", me()),
                ],
            ))
        }
        "closePullRequest"
        | "reopenPullRequest"
        | "markPullRequestReadyForReview"
        | "convertPullRequestToDraft" => {
            let (repo, c) = pull_of(cx, &s("pullRequestId").unwrap_or_default()).await?;
            let svc = crate::rest::changes(cx)?;
            let c = match name {
                "closePullRequest" => svc
                    .close_change(cx.req(bee_proto::v1::CloseChangeRequest {
                        repo: rid(&repo.id),
                        id: c.id.clone(),
                        reason: String::new(),
                    }))
                    .await?
                    .into_inner(),
                "reopenPullRequest" => svc
                    .reopen_change(cx.req(bee_proto::v1::ReopenChangeRequest {
                        repo: rid(&repo.id),
                        id: c.id.clone(),
                    }))
                    .await?
                    .into_inner(),
                "markPullRequestReadyForReview" => svc
                    .mark_change_ready(cx.req(bee_proto::v1::MarkChangeReadyRequest {
                        repo: rid(&repo.id),
                        id: c.id.clone(),
                    }))
                    .await?
                    .into_inner(),
                _ => svc
                    .convert_change_to_draft(cx.req(bee_proto::v1::ConvertChangeToDraftRequest {
                        repo: rid(&repo.id),
                        id: c.id.clone(),
                    }))
                    .await?
                    .into_inner(),
            };
            let ty = format!("{}{}Payload", name[..1].to_uppercase(), &name[1..]);
            Ok(payload(
                &i,
                &ty,
                vec![(
                    "pullRequest",
                    Out::Obj(Node::Pull(Arc::new(repo), Arc::new(c))),
                )],
            ))
        }
        "requestReviews" => {
            let (repo, c) = pull_of(cx, &s("pullRequestId").unwrap_or_default()).await?;
            let mut users = principal_ids(&list("userIds"))?;
            users.extend(principal_ids(&list("botIds"))?);
            let teams: Vec<String> = list("teamIds")
                .iter()
                .map(|t| parse_node(t).map(|n| n.key).ok_or_else(|| not_found(t)))
                .collect::<Result<_, _>>()?;
            let svc = crate::rest::changes(cx)?;
            if i.get("union").and_then(Value::as_bool) != Some(true) {
                let drop_users: Vec<String> = c
                    .review_requests
                    .iter()
                    .filter_map(|rr| rr.reviewer.as_ref().map(|p| p.id.clone()))
                    .filter(|id| !users.contains(id))
                    .collect();
                let drop_teams: Vec<String> = c
                    .review_requests
                    .iter()
                    .map(|rr| rr.team_id.clone())
                    .filter(|t| !t.is_empty() && !teams.contains(t))
                    .collect();
                if !drop_users.is_empty() || !drop_teams.is_empty() {
                    svc.remove_review_requests(cx.req(
                        bee_proto::v1::RemoveReviewRequestsRequest {
                            repo: rid(&repo.id),
                            id: c.id.clone(),
                            reviewer_ids: drop_users,
                            team_ids: drop_teams,
                        },
                    ))
                    .await?;
                }
            }
            if !users.is_empty() || !teams.is_empty() {
                svc.request_reviewers(cx.req(bee_proto::v1::RequestReviewersRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    reviewer_ids: users,
                    team_ids: teams,
                }))
                .await?;
            }
            Ok(payload(
                &i,
                "RequestReviewsPayload",
                vec![
                    ("pullRequest", refetch_pull(cx, &repo, c.number).await),
                    ("actor", me()),
                ],
            ))
        }
        "addPullRequestReview" => {
            let (repo, c) = pull_of(cx, &s("pullRequestId").unwrap_or_default()).await?;
            let mut comments = Vec::new();
            for t in i
                .get("threads")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let line = t.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
                comments.push(bee_proto::v1::ReviewComment {
                    path: t
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .as_bytes()
                        .to_vec(),
                    side: if t.get("side").and_then(Value::as_str) == Some("LEFT") {
                        "old".into()
                    } else {
                        "new".into()
                    },
                    start_line: t
                        .get("startLine")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32)
                        .unwrap_or(line),
                    end_line: line,
                    body: t
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    ..Default::default()
                });
            }
            let event = s("event").unwrap_or_else(|| "COMMENT".into());
            let event = if event == "REQUEST_CHANGES" || event == "APPROVE" || event == "COMMENT" {
                event
            } else {
                "COMMENT".into()
            };
            let rv = pulls::submit_review(
                cx,
                &repo,
                &c,
                &event,
                &s("body").unwrap_or_default(),
                comments,
            )
            .await?;
            let review = review_out(cx, &Arc::new(repo), &Arc::new(c), &rv).await;
            Ok(payload(
                &i,
                "AddPullRequestReviewPayload",
                vec![("pullRequestReview", review)],
            ))
        }
        "enablePullRequestAutoMerge" | "disablePullRequestAutoMerge" => {
            let (repo, c) = pull_of(cx, &s("pullRequestId").unwrap_or_default()).await?;
            let svc = crate::rest::changes(cx)?;
            let c = if name.starts_with("enable") {
                let msg = match (s("commitHeadline"), s("commitBody")) {
                    (Some(h), Some(b)) => format!("{h}\n\n{b}"),
                    (Some(h), None) => h,
                    (None, Some(b)) => b,
                    (None, None) => String::new(),
                };
                svc.enable_auto_merge(cx.req(EnableAutoMergeRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    method: method_of(s("mergeMethod").as_deref()) as i32,
                    message: msg,
                }))
                .await?
                .into_inner()
            } else {
                svc.disable_auto_merge(cx.req(DisableAutoMergeRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                }))
                .await?
                .into_inner()
            };
            let ty = if name.starts_with("enable") {
                "EnablePullRequestAutoMergePayload"
            } else {
                "DisablePullRequestAutoMergePayload"
            };
            Ok(payload(
                &i,
                ty,
                vec![
                    (
                        "pullRequest",
                        Out::Obj(Node::Pull(Arc::new(repo), Arc::new(c))),
                    ),
                    ("actor", me()),
                ],
            ))
        }
        "updatePullRequestBranch" => {
            let (repo, c) = pull_of(cx, &s("pullRequestId").unwrap_or_default()).await?;
            let c = crate::rest::changes(cx)?
                .update_change_base(cx.req(bee_proto::v1::UpdateChangeBaseRequest {
                    repo: rid(&repo.id),
                    id: c.id.clone(),
                    method: if s("updateMethod").as_deref() == Some("REBASE") {
                        LandMethod::Rebase as i32
                    } else {
                        LandMethod::MergeCommit as i32
                    },
                }))
                .await?
                .into_inner();
            Ok(payload(
                &i,
                "UpdatePullRequestBranchPayload",
                vec![(
                    "pullRequest",
                    Out::Obj(Node::Pull(Arc::new(repo), Arc::new(c))),
                )],
            ))
        }
        "addComment" => {
            let (repo, sub) = subject_of(cx, &s("subjectId").unwrap_or_default()).await?;
            let c =
                crate::rest::issues::add_comment(cx, &repo, &sub, &s("body").unwrap_or_default())
                    .await?;
            let (number, is_pr) = match &sub {
                Subject::Issue(x) => (x.number, false),
                Subject::Pull(x) => (x.number, true),
            };
            let parent = subject_out(&repo, sub);
            let node_ = comment_out(ex, &repo, parent.clone(), is_pr, number, &c);
            let edge = Out::map(
                "IssueCommentEdge",
                vec![("cursor", Out::leaf(cursor(0))), ("node", node_)],
            );
            Ok(payload(
                &i,
                "AddCommentPayload",
                vec![
                    ("commentEdge", edge.clone()),
                    ("timelineEdge", edge),
                    ("subject", parent),
                ],
            ))
        }
        "createIssue" => {
            let rid_ = s("repositoryId").unwrap_or_default();
            let n = parse_node(&rid_)
                .filter(|n| n.kind == "Repository")
                .ok_or_else(|| not_found(&rid_))?;
            let repo = repo_by_id(cx, &n.key).await?;
            let issue = cx
                .hub
                .svc
                .create_issue(cx.req(bee_proto::v1::CreateIssueRequest {
                    repo: rid(&repo.id),
                    title: s("title").unwrap_or_default(),
                    body: s("body").unwrap_or_default(),
                    assignees: principal_ids(&list("assigneeIds"))?,
                    labels: label_names(&list("labelIds"))?,
                    milestone_number: match s("milestoneId") {
                        Some(m) => milestone_number(&m)?,
                        None => 0,
                    },
                    template: s("issueTemplate").unwrap_or_default(),
                    ..Default::default()
                }))
                .await?
                .into_inner();
            Ok(payload(
                &i,
                "CreateIssuePayload",
                vec![(
                    "issue",
                    Out::Obj(Node::Issue(Arc::new(repo), Arc::new(issue))),
                )],
            ))
        }
        "updateIssue" => {
            let (repo, issue) = issue_of(cx, &s("id").unwrap_or_default()).await?;
            let mut body = json!({});
            if let Some(t) = s("title") {
                body["title"] = json!(t);
            }
            if let Some(b) = s("body") {
                body["body"] = json!(b);
            }
            if let Some(st) = s("state") {
                body["state"] = json!(st.to_lowercase());
            }
            if i.contains_key("labelIds") {
                body["labels"] = json!(label_names(&list("labelIds"))?);
            }
            if i.contains_key("assigneeIds") {
                body["assignees"] = json!(logins(cx, &principal_ids(&list("assigneeIds"))?).await);
            }
            if let Some(m) = i.get("milestoneId") {
                body["milestone"] = match m.as_str() {
                    Some(id) => json!(milestone_number(id)?),
                    None => Value::Null,
                };
            }
            let issue = crate::rest::issues::update_issue(cx, &repo, issue, &body).await?;
            Ok(payload(
                &i,
                "UpdateIssuePayload",
                vec![
                    (
                        "issue",
                        Out::Obj(Node::Issue(Arc::new(repo), Arc::new(issue))),
                    ),
                    ("actor", me()),
                ],
            ))
        }
        "closeIssue" | "reopenIssue" => {
            let (repo, issue) = issue_of(cx, &s("issueId").unwrap_or_default()).await?;
            let issue = if name == "closeIssue" {
                let reason = match s("stateReason").as_deref() {
                    Some("NOT_PLANNED") => IssueStateReason::NotPlanned,
                    Some("DUPLICATE") => IssueStateReason::Duplicate,
                    _ => IssueStateReason::Completed,
                };
                cx.hub
                    .svc
                    .close_issue(cx.req(bee_proto::v1::CloseIssueRequest {
                        repo: rid(&repo.id),
                        number: issue.number,
                        reason: reason as i32,
                        ..Default::default()
                    }))
                    .await?
                    .into_inner()
            } else {
                cx.hub
                    .svc
                    .reopen_issue(cx.req(bee_proto::v1::ReopenIssueRequest {
                        repo: rid(&repo.id),
                        number: issue.number,
                        ..Default::default()
                    }))
                    .await?
                    .into_inner()
            };
            let ty = if name == "closeIssue" {
                "CloseIssuePayload"
            } else {
                "ReopenIssuePayload"
            };
            Ok(payload(
                &i,
                ty,
                vec![(
                    "issue",
                    Out::Obj(Node::Issue(Arc::new(repo), Arc::new(issue))),
                )],
            ))
        }
        "deleteIssue" => {
            let (repo, issue) = issue_of(cx, &s("issueId").unwrap_or_default()).await?;
            cx.hub
                .svc
                .delete_issue(cx.req(bee_proto::v1::DeleteIssueRequest {
                    repo: rid(&repo.id),
                    number: issue.number,
                }))
                .await?;
            Ok(payload(
                &i,
                "DeleteIssuePayload",
                vec![("repository", Out::Obj(Node::Repo(Arc::new(repo))))],
            ))
        }
        "addLabelsToLabelable" | "removeLabelsFromLabelable" => {
            let (repo, sub) = subject_of(cx, &s("labelableId").unwrap_or_default()).await?;
            let names = label_names(&list("labelIds"))?;
            let (cur, number): (Vec<String>, u64) = match &sub {
                Subject::Issue(x) => (x.labels.iter().map(|l| l.name.clone()).collect(), x.number),
                Subject::Pull(x) => (x.labels.iter().map(|l| l.name.clone()).collect(), x.number),
            };
            let next: Vec<String> = if name.starts_with("add") {
                let mut v = cur;
                for n in names {
                    if !v.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                        v.push(n);
                    }
                }
                v
            } else {
                cur.into_iter()
                    .filter(|x| !names.iter().any(|n| n.eq_ignore_ascii_case(x)))
                    .collect()
            };
            let body = json!({"labels": next});
            match sub {
                Subject::Issue(x) => {
                    crate::rest::issues::update_issue(cx, &repo, *x, &body).await?;
                }
                Subject::Pull(x) => {
                    crate::rest::issues::update_pull(cx, &repo, *x, &body).await?;
                }
            }
            let ty = if name.starts_with("add") {
                "AddLabelsToLabelablePayload"
            } else {
                "RemoveLabelsFromLabelablePayload"
            };
            Ok(payload(
                &i,
                ty,
                vec![("labelable", refetch_subject(cx, &repo, number).await)],
            ))
        }
        "addAssigneesToAssignable" | "removeAssigneesFromAssignable" => {
            let (repo, sub) = subject_of(cx, &s("assignableId").unwrap_or_default()).await?;
            let ids = principal_ids(&list("assigneeIds"))?;
            let (cur, number): (Vec<String>, u64) = match &sub {
                Subject::Issue(x) => (x.assignees.iter().map(|a| a.id.clone()).collect(), x.number),
                Subject::Pull(x) => (x.assignees.iter().map(|a| a.id.clone()).collect(), x.number),
            };
            let next: Vec<String> = if name.starts_with("add") {
                let mut v = cur;
                for id in ids {
                    if !v.contains(&id) {
                        v.push(id);
                    }
                }
                v
            } else {
                cur.into_iter().filter(|x| !ids.contains(x)).collect()
            };
            let body = json!({"assignees": logins(cx, &next).await});
            match sub {
                Subject::Issue(x) => {
                    crate::rest::issues::update_issue(cx, &repo, *x, &body).await?;
                }
                Subject::Pull(x) => {
                    crate::rest::issues::update_pull(cx, &repo, *x, &body).await?;
                }
            }
            let ty = if name.starts_with("add") {
                "AddAssigneesToAssignablePayload"
            } else {
                "RemoveAssigneesFromAssignablePayload"
            };
            Ok(payload(
                &i,
                ty,
                vec![("assignable", refetch_subject(cx, &repo, number).await)],
            ))
        }
        "addStar" | "removeStar" => {
            let id = s("starrableId").unwrap_or_default();
            let n = parse_node(&id)
                .filter(|n| n.kind == "Repository")
                .ok_or_else(|| not_found(&id))?;
            let repo = repo_by_id(cx, &n.key).await?;
            if name == "addStar" {
                cx.hub
                    .svc
                    .star_repo(cx.req(StarRepoRequest {
                        repo: rid(&repo.id),
                    }))
                    .await?;
            } else {
                cx.hub
                    .svc
                    .unstar_repo(cx.req(UnstarRepoRequest {
                        repo: rid(&repo.id),
                    }))
                    .await?;
            }
            let repo = repo_by_id(cx, &n.key).await?;
            let ty = if name == "addStar" {
                "AddStarPayload"
            } else {
                "RemoveStarPayload"
            };
            Ok(payload(
                &i,
                ty,
                vec![("starrable", Out::Obj(Node::Repo(Arc::new(repo))))],
            ))
        }
        "addReaction" => {
            let (repo, sub) = subject_of(cx, &s("subjectId").unwrap_or_default()).await?;
            let word = s("content").unwrap_or_default();
            let content = [
                ReactionContent::ThumbsUp,
                ReactionContent::ThumbsDown,
                ReactionContent::Laugh,
                ReactionContent::Hooray,
                ReactionContent::Confused,
                ReactionContent::Heart,
                ReactionContent::Rocket,
                ReactionContent::Eyes,
            ]
            .into_iter()
            .find(|c| reaction_word(*c) == word)
            .ok_or_else(|| GqlError::new("UNPROCESSABLE", format!("unknown reaction {word}")))?;
            let fcx = crate::rest::issues::authorize(cx, &repo, bee_policy::Action::Read).await?;
            let number = match &sub {
                Subject::Pull(c) => {
                    crate::rest::changes(cx)?
                        .changes()
                        .react(&fcx, &repo, &c.id, content, true)
                        .await
                        .map_err(ApiError::from)?;
                    c.number
                }
                Subject::Issue(x) => {
                    let actor_ = bee_forge::work::Actor {
                        principal: cx.principal(&fcx.actor_id).await,
                        triage: false,
                        admin: false,
                        association: "MEMBER".into(),
                    };
                    cx.hub
                        .forge
                        .work
                        .add_reaction(
                            &fcx,
                            &bee_forge::work::RepoInfo::from(&repo),
                            &actor_,
                            &x.id,
                            content,
                        )
                        .await
                        .map_err(ApiError::from)?;
                    x.number
                }
            };
            let reaction = Out::map(
                "Reaction",
                vec![
                    (
                        "id",
                        Out::leaf(node("Reaction", &format!("{}:{number}:{word}", repo.id))),
                    ),
                    ("content", Out::leaf(word)),
                    ("createdAt", Out::Null),
                    ("user", me()),
                ],
            );
            Ok(payload(
                &i,
                "AddReactionPayload",
                vec![
                    ("subject", refetch_subject(cx, &repo, number).await),
                    ("reaction", reaction),
                ],
            ))
        }
        "lockLockable" | "unlockLockable" => {
            let (repo, sub) = subject_of(cx, &s("lockableId").unwrap_or_default()).await?;
            let lock = name == "lockLockable";
            let number = match &sub {
                Subject::Issue(x) => {
                    if lock {
                        cx.hub
                            .svc
                            .lock_issue(cx.req(bee_proto::v1::LockIssueRequest {
                                repo: rid(&repo.id),
                                number: x.number,
                                reason: 0,
                            }))
                            .await?;
                    } else {
                        cx.hub
                            .svc
                            .unlock_issue(cx.req(bee_proto::v1::UnlockIssueRequest {
                                repo: rid(&repo.id),
                                number: x.number,
                            }))
                            .await?;
                    }
                    x.number
                }
                Subject::Pull(c) => {
                    let fcx = crate::rest::issues::authorize(cx, &repo, bee_policy::Action::Triage)
                        .await?;
                    crate::rest::changes(cx)?
                        .changes()
                        .set_locked(&fcx, &repo, &c.id, lock)
                        .await
                        .map_err(ApiError::from)?;
                    c.number
                }
            };
            let (ty, key) = if lock {
                ("LockLockablePayload", "lockedRecord")
            } else {
                ("UnlockLockablePayload", "unlockedRecord")
            };
            Ok(payload(
                &i,
                ty,
                vec![(key, refetch_subject(cx, &repo, number).await)],
            ))
        }
        _ => Err(GqlError::new(
            "UNDEFINED_FIELD",
            format!("Field '{name}' doesn't exist on type 'Mutation'"),
        )),
    }
}
