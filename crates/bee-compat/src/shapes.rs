//! GitHub's REST object shapes, built from forge records.
//!
//! Field sets follow GitHub's REST reference (API version 2022-11-28); fields bee has no
//! data for are present with GitHub's empty value (`null`, `false`, `0`, `""`) so typed
//! clients deserialize them. Every numeric id comes from [`crate::ids::num`].

use bee_proto::v1::{
    ChangeRequest, ChangeState, Comment, Issue, IssueState, IssueStateReason, Label, Milestone,
    MilestoneState, Org, Principal, PrincipalKind, ReactionContent, ReactionGroup, Repo, RepoRole,
    RepoSettings, Visibility,
};
use serde_json::{Value, json};

use crate::http::Ctx;
use crate::ids::node;
use crate::ports::{GitCommitInfo, Signature};
use crate::time::{iso, iso_or, iso_secs};

/// `owner` and `name` of a repo.
pub(crate) fn split(repo: &Repo) -> (&str, &str) {
    repo.full_name
        .split_once('/')
        .unwrap_or(("", repo.name.as_str()))
}

fn gh_type(p: &Principal) -> &'static str {
    match PrincipalKind::try_from(p.kind).unwrap_or(PrincipalKind::Unspecified) {
        PrincipalKind::Agent | PrincipalKind::Service => "Bot",
        _ => "User",
    }
}

/// The URL fields of a user-like account.
fn account_urls(cx: &Ctx, login: &str) -> Value {
    let base = cx.api(&format!("/users/{login}"));
    json!({
        "url": base,
        "html_url": cx.html(&format!("/{login}")),
        "followers_url": format!("{base}/followers"),
        "following_url": format!("{base}/following{{/other_user}}"),
        "gists_url": format!("{base}/gists{{/gist_id}}"),
        "starred_url": format!("{base}/starred{{/owner}}{{/repo}}"),
        "subscriptions_url": format!("{base}/subscriptions"),
        "organizations_url": format!("{base}/orgs"),
        "repos_url": format!("{base}/repos"),
        "events_url": format!("{base}/events{{/privacy}}"),
        "received_events_url": format!("{base}/received_events"),
        "avatar_url": cx.html(&format!("/{login}.png")),
    })
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Value::Object(m), Value::Object(n)) = (&mut a, b) {
        m.extend(n);
    }
    a
}

/// GitHub's "simple user" for a principal (`null` for an empty one).
pub(crate) async fn user(cx: &Ctx, p: &Principal) -> Value {
    if p.id.is_empty() {
        return Value::Null;
    }
    let p = if p.display.is_empty() && p.kind == 0 {
        cx.principal(&p.id).await
    } else {
        p.clone()
    };
    let login = cx.login(&p).await;
    let kind = gh_type(&p);
    let id = cx.hub.ids.remember("user", &p.id, "");
    merge(
        json!({
            "login": login,
            "id": id,
            "node_id": node(if kind == "Bot" { "Bot" } else { "User" }, &p.id),
            "gravatar_id": "",
            "type": kind,
            "site_admin": false,
            "user_view_type": "public",
        }),
        account_urls(cx, &login),
    )
}

/// Users for a list of principals.
pub(crate) async fn users(cx: &Ctx, ps: &[Principal]) -> Vec<Value> {
    let mut out = Vec::with_capacity(ps.len());
    for p in ps {
        out.push(user(cx, p).await);
    }
    out
}

/// An org shown where GitHub shows an account (`owner` of a repo).
pub(crate) fn org_owner(cx: &Ctx, name: &str, org_id: &str) -> Value {
    let id = cx.hub.ids.remember("org", org_id, name);
    merge(
        json!({
            "login": name,
            "id": id,
            "node_id": node("Organization", org_id),
            "gravatar_id": "",
            "type": "Organization",
            "site_admin": false,
            "user_view_type": "public",
        }),
        account_urls(cx, name),
    )
}

/// GitHub's organization object.
pub(crate) fn org(cx: &Ctx, o: &Org) -> Value {
    let base = cx.api(&format!("/orgs/{}", o.name));
    json!({
        "login": o.name,
        "id": cx.hub.ids.remember("org", &o.id, &o.name),
        "node_id": node("Organization", &o.id),
        "url": base,
        "repos_url": format!("{base}/repos"),
        "events_url": format!("{base}/events"),
        "hooks_url": format!("{base}/hooks"),
        "issues_url": format!("{base}/issues"),
        "members_url": format!("{base}/members{{/member}}"),
        "public_members_url": format!("{base}/public_members{{/member}}"),
        "avatar_url": if o.avatar_url.is_empty() { cx.html(&format!("/{}.png", o.name)) } else { o.avatar_url.clone() },
        "description": o.description,
        "name": if o.display.is_empty() { o.name.clone() } else { o.display.clone() },
        "company": Value::Null,
        "blog": o.website,
        "location": o.location,
        "email": o.email,
        "is_verified": !o.verified_domains.is_empty(),
        "has_organization_projects": true,
        "has_repository_projects": true,
        "html_url": cx.html(&format!("/{}", o.name)),
        "type": "Organization",
        "created_at": iso(&o.created_at),
        "updated_at": iso(&o.created_at),
        "archived_at": Value::Null,
    })
}

/// `permissions` for a repo role.
pub(crate) fn permissions(role: RepoRole) -> Value {
    let r = role as i32;
    json!({
        "admin": r >= RepoRole::Admin as i32,
        "maintain": r >= RepoRole::Maintain as i32,
        "push": r >= RepoRole::Write as i32,
        "triage": r >= RepoRole::Triage as i32,
        "pull": r >= RepoRole::Read as i32,
    })
}

/// The visibility word.
pub(crate) fn visibility(repo: &Repo) -> &'static str {
    match Visibility::try_from(repo.visibility).unwrap_or(Visibility::Private) {
        Visibility::Public => "public",
        Visibility::Internal => "internal",
        _ => "private",
    }
}

/// GitHub's repository object. `settings` fills the merge options; `role` the
/// `permissions` block.
pub(crate) fn repo(
    cx: &Ctx,
    r: &Repo,
    settings: Option<&RepoSettings>,
    role: Option<RepoRole>,
) -> Value {
    let (owner, name) = split(r);
    let base = cx.api(&format!("/repos/{}", r.full_name));
    let html = cx.html(&format!("/{}", r.full_name));
    let id = cx.hub.ids.remember("repo", &r.id, &r.full_name);
    let allowed = |m: bee_proto::v1::LandMethod| {
        settings.is_none_or(|s| {
            s.allowed_land_methods.is_empty() || s.allowed_land_methods.contains(&(m as i32))
        })
    };
    let flag = |f: fn(&RepoSettings) -> bool, default: bool| settings.map(f).unwrap_or(default);
    let mut v = json!({
        "id": id,
        "node_id": node("Repository", &r.id),
        "name": name,
        "full_name": r.full_name,
        "private": visibility(r) != "public",
        "owner": org_owner(cx, owner, &r.org_id),
        "html_url": html,
        "description": if r.description.is_empty() { Value::Null } else { Value::String(r.description.clone()) },
        "fork": !r.fork_parent_id.is_empty(),
        "url": base,
        "forks_url": format!("{base}/forks"),
        "keys_url": format!("{base}/keys{{/key_id}}"),
        "collaborators_url": format!("{base}/collaborators{{/collaborator}}"),
        "teams_url": format!("{base}/teams"),
        "hooks_url": format!("{base}/hooks"),
        "issue_events_url": format!("{base}/issues/events{{/number}}"),
        "events_url": format!("{base}/events"),
        "assignees_url": format!("{base}/assignees{{/user}}"),
        "branches_url": format!("{base}/branches{{/branch}}"),
        "tags_url": format!("{base}/tags"),
        "blobs_url": format!("{base}/git/blobs{{/sha}}"),
        "git_tags_url": format!("{base}/git/tags{{/sha}}"),
        "git_refs_url": format!("{base}/git/refs{{/sha}}"),
        "trees_url": format!("{base}/git/trees{{/sha}}"),
        "statuses_url": format!("{base}/statuses/{{sha}}"),
        "languages_url": format!("{base}/languages"),
        "stargazers_url": format!("{base}/stargazers"),
        "contributors_url": format!("{base}/contributors"),
        "subscribers_url": format!("{base}/subscribers"),
        "subscription_url": format!("{base}/subscription"),
        "commits_url": format!("{base}/commits{{/sha}}"),
        "git_commits_url": format!("{base}/git/commits{{/sha}}"),
        "comments_url": format!("{base}/comments{{/number}}"),
        "issue_comment_url": format!("{base}/issues/comments{{/number}}"),
        "contents_url": format!("{base}/contents/{{+path}}"),
        "compare_url": format!("{base}/compare/{{base}}...{{head}}"),
        "merges_url": format!("{base}/merges"),
        "archive_url": format!("{base}/{{archive_format}}{{/ref}}"),
        "downloads_url": format!("{base}/downloads"),
        "issues_url": format!("{base}/issues{{/number}}"),
        "pulls_url": format!("{base}/pulls{{/number}}"),
        "milestones_url": format!("{base}/milestones{{/number}}"),
        "notifications_url": format!("{base}/notifications{{?since,all,participating}}"),
        "labels_url": format!("{base}/labels{{/name}}"),
        "releases_url": format!("{base}/releases{{/id}}"),
        "deployments_url": format!("{base}/deployments"),
        "created_at": iso(&r.created_at),
        "updated_at": iso_or(&r.pushed_at, &r.created_at),
        "pushed_at": iso_or(&r.pushed_at, &r.created_at),
        "git_url": format!("{html}.git"),
        "ssh_url": format!("{html}.git"),
        "clone_url": format!("{html}.git"),
        "svn_url": html,
        "homepage": if r.homepage.is_empty() { Value::Null } else { Value::String(r.homepage.clone()) },
        "size": r.size_bytes / 1024,
        "stargazers_count": r.stars,
        "watchers_count": r.stars,
        "language": if r.primary_language.is_empty() { Value::Null } else { Value::String(r.primary_language.clone()) },
        "has_issues": flag(|s| s.has_issues, true),
        "has_projects": flag(|s| s.has_projects, true),
        "has_downloads": true,
        "has_wiki": flag(|s| s.has_wiki, false),
        "has_pages": false,
        "has_discussions": flag(|s| s.has_discussions, false),
        "forks_count": r.forks,
        "mirror_url": Value::Null,
        "archived": r.archived,
        "disabled": false,
        "open_issues_count": r.open_issues + r.open_changes,
        "license": if r.license_spdx.is_empty() { Value::Null } else { json!({
            "key": r.license_spdx.to_lowercase(), "name": r.license_spdx, "spdx_id": r.license_spdx,
            "url": Value::Null, "node_id": node("License", &r.license_spdx),
        }) },
        "allow_forking": flag(|s| s.allow_forking, true),
        "is_template": r.is_template,
        "web_commit_signoff_required": flag(|s| s.web_commit_signoff_required, false),
        "topics": r.topics,
        "visibility": visibility(r),
        "forks": r.forks,
        "open_issues": r.open_issues + r.open_changes,
        "watchers": r.stars,
        "default_branch": if r.default_ref.is_empty() { "main" } else { r.default_ref.as_str() },
        "allow_squash_merge": allowed(bee_proto::v1::LandMethod::Squash),
        "allow_merge_commit": allowed(bee_proto::v1::LandMethod::MergeCommit),
        "allow_rebase_merge": allowed(bee_proto::v1::LandMethod::Rebase),
        "allow_auto_merge": flag(|s| s.allow_auto_merge, false),
        "delete_branch_on_merge": flag(|s| s.delete_workspace_ref_on_land, false),
        "allow_update_branch": flag(|s| s.suggest_update_base, false),
        "use_squash_pr_title_as_default": false,
        "squash_merge_commit_message": "COMMIT_MESSAGES",
        "squash_merge_commit_title": "COMMIT_OR_PR_TITLE",
        "merge_commit_message": "PR_TITLE",
        "merge_commit_title": "MERGE_MESSAGE",
        "network_count": r.forks,
        "subscribers_count": 0,
    });
    if let Some(role) = role {
        v["permissions"] = permissions(role);
    }
    v
}

/// GitHub's reaction rollup.
pub(crate) fn reactions(url: &str, groups: &[ReactionGroup]) -> Value {
    let mut v = json!({
        "url": url, "total_count": 0, "+1": 0, "-1": 0, "laugh": 0, "hooray": 0,
        "confused": 0, "heart": 0, "rocket": 0, "eyes": 0,
    });
    let mut total = 0u64;
    for g in groups {
        let key = reaction_name(ReactionContent::try_from(g.content).unwrap_or_default());
        if !key.is_empty() {
            v[key] = json!(g.count);
            total += g.count as u64;
        }
    }
    v["total_count"] = json!(total);
    v
}

/// GitHub's reaction word.
pub(crate) fn reaction_name(c: ReactionContent) -> &'static str {
    match c {
        ReactionContent::ThumbsUp => "+1",
        ReactionContent::ThumbsDown => "-1",
        ReactionContent::Laugh => "laugh",
        ReactionContent::Hooray => "hooray",
        ReactionContent::Confused => "confused",
        ReactionContent::Heart => "heart",
        ReactionContent::Rocket => "rocket",
        ReactionContent::Eyes => "eyes",
        ReactionContent::Unspecified => "",
    }
}

/// The reaction for GitHub's word.
pub(crate) fn reaction_of(word: &str) -> Option<ReactionContent> {
    Some(match word {
        "+1" => ReactionContent::ThumbsUp,
        "-1" => ReactionContent::ThumbsDown,
        "laugh" => ReactionContent::Laugh,
        "hooray" => ReactionContent::Hooray,
        "confused" => ReactionContent::Confused,
        "heart" => ReactionContent::Heart,
        "rocket" => ReactionContent::Rocket,
        "eyes" => ReactionContent::Eyes,
        _ => return None,
    })
}

/// GitHub's label object.
pub(crate) fn label(cx: &Ctx, repo: &Repo, l: &Label) -> Value {
    json!({
        "id": cx.hub.ids.remember("label", &l.id, &repo.id),
        "node_id": node("Label", &format!("{}:{}", repo.id, l.name)),
        "url": cx.api(&format!("/repos/{}/labels/{}", repo.full_name, crate::http::encode(&l.name))),
        "name": l.name,
        "color": l.color,
        "default": l.is_default,
        "description": if l.description.is_empty() { Value::Null } else { Value::String(l.description.clone()) },
    })
}

/// GitHub's milestone object.
pub(crate) fn milestone(cx: &Ctx, repo: &Repo, m: &Milestone) -> Value {
    if m.id.is_empty() && m.number == 0 {
        return Value::Null;
    }
    let base = cx.api(&format!(
        "/repos/{}/milestones/{}",
        repo.full_name, m.number
    ));
    let closed = MilestoneState::try_from(m.state).unwrap_or_default() == MilestoneState::Closed;
    json!({
        "url": base,
        "html_url": cx.html(&format!("/{}/milestone/{}", repo.full_name, m.number)),
        "labels_url": format!("{base}/labels"),
        "id": cx.hub.ids.remember("milestone", &m.id, &repo.id),
        "node_id": node("Milestone", &format!("{}:{}", repo.id, m.number)),
        "number": m.number,
        "state": if closed { "closed" } else { "open" },
        "title": m.title,
        "description": if m.description.is_empty() { Value::Null } else { Value::String(m.description.clone()) },
        "creator": Value::Null,
        "open_issues": m.open_items,
        "closed_issues": m.closed_items,
        "created_at": iso(&m.created_at),
        "updated_at": iso_or(&m.closed_at, &m.created_at),
        "closed_at": iso(&m.closed_at),
        "due_on": iso(&m.due_on),
    })
}

fn opt_text(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::String(s.to_string())
    }
}

/// GitHub's issue object for an issue.
pub(crate) async fn issue(cx: &Ctx, repo: &Repo, i: &Issue) -> Value {
    let base = cx.api(&format!("/repos/{}/issues/{}", repo.full_name, i.number));
    let closed = IssueState::try_from(i.state).unwrap_or_default() == IssueState::Closed;
    let reason = match IssueStateReason::try_from(i.state_reason).unwrap_or_default() {
        IssueStateReason::Completed => "completed",
        IssueStateReason::NotPlanned => "not_planned",
        IssueStateReason::Duplicate => "duplicate",
        IssueStateReason::Reopened => "reopened",
        IssueStateReason::Unspecified => "",
    };
    let assignees = users(cx, &i.assignees).await;
    json!({
        "id": cx.hub.ids.remember("issue", &i.id, &format!("{}:{}", repo.id, i.number)),
        "node_id": node("Issue", &format!("{}:{}", repo.id, i.number)),
        "url": base,
        "repository_url": cx.api(&format!("/repos/{}", repo.full_name)),
        "labels_url": format!("{base}/labels{{/name}}"),
        "comments_url": format!("{base}/comments"),
        "events_url": format!("{base}/events"),
        "timeline_url": format!("{base}/timeline"),
        "html_url": cx.html(&format!("/{}/issues/{}", repo.full_name, i.number)),
        "number": i.number,
        "state": if closed { "closed" } else { "open" },
        "state_reason": opt_text(reason),
        "title": i.title,
        "body": opt_text(&i.body),
        "user": user(cx, i.author.as_ref().unwrap_or(&Principal::default())).await,
        "labels": i.labels.iter().map(|l| label(cx, repo, l)).collect::<Vec<_>>(),
        "assignee": assignees.first().cloned().unwrap_or(Value::Null),
        "assignees": assignees,
        "milestone": i.milestone.as_ref().map(|m| milestone(cx, repo, m)).unwrap_or(Value::Null),
        "locked": i.locked,
        "active_lock_reason": Value::Null,
        "comments": i.comment_count,
        "closed_at": iso(&i.closed_at),
        "created_at": iso(&i.created_at),
        "updated_at": iso_or(&i.updated_at, &i.created_at),
        "closed_by": match &i.closed_by { Some(p) => user(cx, p).await, None => Value::Null },
        "author_association": if i.author_association.is_empty() { "NONE" } else { i.author_association.as_str() },
        "reactions": reactions(&format!("{base}/reactions"), &i.reactions),
        "performed_via_github_app": Value::Null,
    })
}

/// `open` or `closed`, and whether landed.
pub(crate) fn cr_state(c: &ChangeRequest) -> (&'static str, bool) {
    match ChangeState::try_from(c.state).unwrap_or_default() {
        ChangeState::Landed => ("closed", true),
        ChangeState::Closed => ("closed", false),
        _ => ("open", false),
    }
}

/// GitHub's issue object for a pull request (the issues API view).
pub(crate) async fn issue_of_pull(cx: &Ctx, repo: &Repo, c: &ChangeRequest) -> Value {
    let base = cx.api(&format!("/repos/{}/issues/{}", repo.full_name, c.number));
    let (state, merged) = cr_state(c);
    let assignees = users(cx, &c.assignees).await;
    let html = cx.html(&format!("/{}/pull/{}", repo.full_name, c.number));
    json!({
        "id": cx.hub.ids.remember("issue", &c.id, &format!("{}:{}", repo.id, c.number)),
        "node_id": node("PullRequest", &format!("{}:{}", repo.id, c.number)),
        "url": base,
        "repository_url": cx.api(&format!("/repos/{}", repo.full_name)),
        "labels_url": format!("{base}/labels{{/name}}"),
        "comments_url": format!("{base}/comments"),
        "events_url": format!("{base}/events"),
        "timeline_url": format!("{base}/timeline"),
        "html_url": html,
        "number": c.number,
        "state": state,
        "state_reason": Value::Null,
        "title": c.title,
        "body": opt_text(&c.body),
        "user": user(cx, c.author.as_ref().unwrap_or(&Principal::default())).await,
        "labels": c.labels.iter().map(|l| label(cx, repo, l)).collect::<Vec<_>>(),
        "assignee": assignees.first().cloned().unwrap_or(Value::Null),
        "assignees": assignees,
        "milestone": c.milestone.as_ref().map(|m| milestone(cx, repo, m)).unwrap_or(Value::Null),
        "locked": c.locked,
        "active_lock_reason": Value::Null,
        "comments": c.comment_count,
        "draft": c.draft,
        "pull_request": {
            "url": cx.api(&format!("/repos/{}/pulls/{}", repo.full_name, c.number)),
            "html_url": html,
            "diff_url": format!("{html}.diff"),
            "patch_url": format!("{html}.patch"),
            "merged_at": if merged { iso_or(&c.landed_at, &c.closed_at) } else { Value::Null },
        },
        "closed_at": iso_or(&c.closed_at, &c.landed_at),
        "created_at": iso(&c.created_at),
        "updated_at": iso_or(&c.updated_at, &c.created_at),
        "closed_by": Value::Null,
        "author_association": "MEMBER",
        "reactions": reactions(&format!("{base}/reactions"), &c.reactions),
        "performed_via_github_app": Value::Null,
    })
}

/// GitHub's issue comment object.
pub(crate) async fn comment(cx: &Ctx, repo: &Repo, number: u64, c: &Comment) -> Value {
    let id = cx
        .hub
        .ids
        .remember("comment", &c.id, &format!("{}:{}", repo.id, number));
    let url = cx.api(&format!("/repos/{}/issues/comments/{id}", repo.full_name));
    json!({
        "id": id,
        "node_id": node("IssueComment", &c.id),
        "url": url,
        "html_url": cx.html(&format!("/{}/issues/{number}#issuecomment-{id}", repo.full_name)),
        "issue_url": cx.api(&format!("/repos/{}/issues/{number}", repo.full_name)),
        "body": c.body,
        "user": user(cx, c.author.as_ref().unwrap_or(&Principal::default())).await,
        "created_at": iso(&c.created_at),
        "updated_at": iso_or(&c.updated_at, &c.created_at),
        "author_association": if c.author_association.is_empty() { "NONE" } else { c.author_association.as_str() },
        "reactions": reactions(&format!("{url}/reactions"), &c.reactions),
        "performed_via_github_app": Value::Null,
    })
}

/// A git identity in GitHub's `commit.author` form.
pub(crate) fn signature(s: &Signature) -> Value {
    json!({"name": s.name, "email": s.email, "date": iso_secs(s.time_secs)})
}

/// GitHub's git commit object (`/git/commits/{sha}`).
pub(crate) fn git_commit(cx: &Ctx, repo: &Repo, c: &GitCommitInfo) -> Value {
    let base = cx.api(&format!("/repos/{}", repo.full_name));
    json!({
        "sha": c.sha,
        "node_id": node("Commit", &format!("{}:{}", repo.id, c.sha)),
        "url": format!("{base}/git/commits/{}", c.sha),
        "html_url": cx.html(&format!("/{}/commit/{}", repo.full_name, c.sha)),
        "author": signature(&c.author),
        "committer": signature(&c.committer),
        "message": c.message,
        "tree": {"sha": c.tree, "url": format!("{base}/git/trees/{}", c.tree)},
        "parents": c.parents.iter().map(|p| json!({
            "sha": p, "url": format!("{base}/git/commits/{p}"),
            "html_url": cx.html(&format!("/{}/commit/{p}", repo.full_name)),
        })).collect::<Vec<_>>(),
        "verification": match &c.verification {
            Some(v) => json!({"verified": v.verified, "reason": v.reason, "signature": v.signature, "payload": v.payload, "verified_at": Value::Null}),
            None => json!({"verified": false, "reason": "unsigned", "signature": Value::Null, "payload": Value::Null, "verified_at": Value::Null}),
        },
    })
}

/// GitHub's commit object (`/commits/{sha}`), with the account matched by email when
/// the directory knows it.
pub(crate) async fn commit(cx: &Ctx, repo: &Repo, c: &GitCommitInfo) -> Value {
    let base = cx.api(&format!("/repos/{}", repo.full_name));
    let who = |s: &Signature| {
        let name = s.name.clone();
        async move {
            match cx.by_login(&name).await {
                Ok(p) => user(cx, &p).await,
                Err(_) => Value::Null,
            }
        }
    };
    let mut gc = git_commit(cx, repo, c);
    if let Value::Object(m) = &mut gc {
        m.remove("sha");
        m.remove("node_id");
        m.remove("html_url");
        m.remove("parents");
        m.insert("comment_count".into(), json!(0));
    }
    json!({
        "sha": c.sha,
        "node_id": node("Commit", &format!("{}:{}", repo.id, c.sha)),
        "url": format!("{base}/commits/{}", c.sha),
        "html_url": cx.html(&format!("/{}/commit/{}", repo.full_name, c.sha)),
        "comments_url": format!("{base}/commits/{}/comments", c.sha),
        "commit": gc,
        "author": who(&c.author).await,
        "committer": who(&c.committer).await,
        "parents": c.parents.iter().map(|p| json!({
            "sha": p, "url": format!("{base}/commits/{p}"),
            "html_url": cx.html(&format!("/{}/commit/{p}", repo.full_name)),
        })).collect::<Vec<_>>(),
    })
}
