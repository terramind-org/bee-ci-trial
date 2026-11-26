//! The subset of GitHub's REST and GraphQL objects the importer reads, and [`Metadata`], the
//! whole fetched picture of one repository.
//!
//! Fields GitHub sends that we do not read are ignored; missing optional fields default. Every
//! collection in [`Metadata`] is an ordered map keyed by GitHub's number or id, so merging a
//! page twice (a resumed import, a webhook and a poll seeing the same change) is idempotent
//! and iteration order never depends on arrival order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// GitHub's numeric id of the `ghost` account that stands in for deleted users.
pub const GHOST_ID: u64 = 10137;

/// A GitHub account (user, bot or organization).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Numeric id (stable across renames).
    pub id: u64,
    /// Login at the time of the fetch.
    pub login: String,
    /// `User`, `Bot` or `Organization`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Display name, when the endpoint includes it.
    #[serde(default)]
    pub name: Option<String>,
}

impl User {
    /// The account GitHub shows for deleted users.
    pub fn ghost() -> User {
        User {
            id: GHOST_ID,
            login: "ghost".into(),
            kind: "User".into(),
            name: None,
        }
    }

    /// True for bot accounts (they become agent principals).
    pub fn is_bot(&self) -> bool {
        self.kind == "Bot" || self.login.ends_with("[bot]")
    }
}

/// The repository itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    /// Numeric id.
    pub id: u64,
    /// GraphQL node id.
    #[serde(default)]
    pub node_id: String,
    /// Name.
    pub name: String,
    /// `owner/name`.
    pub full_name: String,
    /// Default branch name.
    #[serde(default)]
    pub default_branch: String,
    /// Private repository.
    #[serde(default)]
    pub private: bool,
    /// Archived on GitHub.
    #[serde(default)]
    pub archived: bool,
    /// The wiki is enabled.
    #[serde(default)]
    pub has_wiki: bool,
    /// `https://github.com/owner/name`.
    #[serde(default)]
    pub html_url: String,
    /// The git clone URL.
    #[serde(default)]
    pub clone_url: String,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
}

/// A label definition (or a label on an issue or PR).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    /// Name.
    pub name: String,
    /// Hex color without `#`.
    #[serde(default)]
    pub color: String,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
}

/// A milestone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    /// Number (per repository).
    pub number: u64,
    /// Title.
    pub title: String,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
    /// `open` or `closed`.
    #[serde(default)]
    pub state: String,
    /// Due date.
    #[serde(default)]
    pub due_on: Option<String>,
    /// When it was closed.
    #[serde(default)]
    pub closed_at: Option<String>,
}

/// The reaction summary GitHub embeds in issues and comments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionSummary {
    /// Total reactions; zero means no per-user fetch is needed.
    #[serde(default)]
    pub total_count: u64,
}

/// An issue (GitHub lists pull requests as issues too; those carry `pull_request`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Number, shared with pull requests.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Body (markdown).
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    #[serde(default)]
    pub state: String,
    /// `completed`, `not_planned`, `reopened`.
    #[serde(default)]
    pub state_reason: Option<String>,
    /// Author (`None` for deleted accounts).
    #[serde(default)]
    pub user: Option<User>,
    /// Labels.
    #[serde(default)]
    pub labels: Vec<Label>,
    /// Assignees.
    #[serde(default)]
    pub assignees: Vec<User>,
    /// Milestone.
    #[serde(default)]
    pub milestone: Option<Milestone>,
    /// Conversation locked.
    #[serde(default)]
    pub locked: bool,
    /// Creation time (RFC 3339).
    #[serde(default)]
    pub created_at: String,
    /// Last update (RFC 3339).
    #[serde(default)]
    pub updated_at: String,
    /// Close time.
    #[serde(default)]
    pub closed_at: Option<String>,
    /// Present when this "issue" is a pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<serde_json::Value>,
    /// Reaction summary.
    #[serde(default)]
    pub reactions: Option<ReactionSummary>,
}

/// One end of a pull request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrEnd {
    /// Branch name.
    #[serde(rename = "ref", default)]
    pub branch: String,
    /// Commit.
    #[serde(default)]
    pub sha: String,
    /// The repository (`None` when a fork was deleted).
    #[serde(default)]
    pub repo: Option<RepoName>,
}

/// A repository reference inside a PR end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoName {
    /// `owner/name`.
    #[serde(default)]
    pub full_name: String,
}

/// A pull request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// Number, shared with issues.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Body.
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    #[serde(default)]
    pub state: String,
    /// Draft.
    #[serde(default)]
    pub draft: bool,
    /// Conversation locked.
    #[serde(default)]
    pub locked: bool,
    /// Merge time (merged PRs only).
    #[serde(default)]
    pub merged_at: Option<String>,
    /// The merge, squash or rebase result commit.
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    /// Author.
    #[serde(default)]
    pub user: Option<User>,
    /// Labels.
    #[serde(default)]
    pub labels: Vec<Label>,
    /// Assignees.
    #[serde(default)]
    pub assignees: Vec<User>,
    /// Requested reviewers still pending.
    #[serde(default)]
    pub requested_reviewers: Vec<User>,
    /// Milestone.
    #[serde(default)]
    pub milestone: Option<Milestone>,
    /// Source branch.
    #[serde(default)]
    pub head: PrEnd,
    /// Target branch.
    #[serde(default)]
    pub base: PrEnd,
    /// Creation time.
    #[serde(default)]
    pub created_at: String,
    /// Last update.
    #[serde(default)]
    pub updated_at: String,
    /// Close time.
    #[serde(default)]
    pub closed_at: Option<String>,
}

impl PullRequest {
    /// True when GitHub merged it.
    pub fn merged(&self) -> bool {
        self.merged_at.is_some()
    }
}

/// A submitted review.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// Id.
    pub id: u64,
    /// Reviewer.
    #[serde(default)]
    pub user: Option<User>,
    /// Body.
    #[serde(default)]
    pub body: Option<String>,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED` or `PENDING`.
    #[serde(default)]
    pub state: String,
    /// The head the review was made against.
    #[serde(default)]
    pub commit_id: Option<String>,
    /// Submission time.
    #[serde(default)]
    pub submitted_at: Option<String>,
}

/// A review (line or file) comment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// Id.
    pub id: u64,
    /// The review it belongs to.
    #[serde(default)]
    pub pull_request_review_id: Option<u64>,
    /// Author.
    #[serde(default)]
    pub user: Option<User>,
    /// Body.
    #[serde(default)]
    pub body: String,
    /// File path.
    #[serde(default)]
    pub path: String,
    /// Head the comment is currently shown on.
    #[serde(default)]
    pub commit_id: String,
    /// Head the comment was made on.
    #[serde(default)]
    pub original_commit_id: String,
    /// Current line (null when outdated).
    #[serde(default)]
    pub line: Option<u32>,
    /// Line on the original commit.
    #[serde(default)]
    pub original_line: Option<u32>,
    /// First line of a multi-line comment.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// First line on the original commit.
    #[serde(default)]
    pub original_start_line: Option<u32>,
    /// `LEFT` (old) or `RIGHT` (new).
    #[serde(default)]
    pub side: Option<String>,
    /// `line` or `file`.
    #[serde(default)]
    pub subject_type: Option<String>,
    /// The diff hunk around the anchor.
    #[serde(default)]
    pub diff_hunk: String,
    /// The comment this one replies to.
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    /// Creation time.
    #[serde(default)]
    pub created_at: String,
    /// Last update.
    #[serde(default)]
    pub updated_at: String,
    /// `https://api.github.com/repos/o/r/pulls/<n>`.
    #[serde(default)]
    pub pull_request_url: String,
    /// Reaction summary.
    #[serde(default)]
    pub reactions: Option<ReactionSummary>,
}

impl ReviewComment {
    /// The pull request number, from `pull_request_url`.
    pub fn pr_number(&self) -> Option<u64> {
        last_number(&self.pull_request_url)
    }
}

/// A conversation comment on an issue or PR.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    /// Id.
    pub id: u64,
    /// Author.
    #[serde(default)]
    pub user: Option<User>,
    /// Body.
    #[serde(default)]
    pub body: String,
    /// Creation time.
    #[serde(default)]
    pub created_at: String,
    /// Last update.
    #[serde(default)]
    pub updated_at: String,
    /// `https://api.github.com/repos/o/r/issues/<n>`.
    #[serde(default)]
    pub issue_url: String,
    /// Reaction summary.
    #[serde(default)]
    pub reactions: Option<ReactionSummary>,
}

impl IssueComment {
    /// The issue or PR number, from `issue_url`.
    pub fn number(&self) -> Option<u64> {
        last_number(&self.issue_url)
    }
}

/// One user's reaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    /// Id.
    pub id: u64,
    /// Who reacted.
    #[serde(default)]
    pub user: Option<User>,
    /// `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket` or `eyes`.
    pub content: String,
}

/// A release asset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// Id.
    pub id: u64,
    /// File name.
    pub name: String,
    /// Label.
    #[serde(default)]
    pub label: Option<String>,
    /// MIME type.
    #[serde(default)]
    pub content_type: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Public download URL.
    #[serde(default)]
    pub browser_download_url: String,
    /// API URL (download with `Accept: application/octet-stream`).
    #[serde(default)]
    pub url: String,
}

/// A release.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Id.
    pub id: u64,
    /// Tag.
    pub tag_name: String,
    /// Title.
    #[serde(default)]
    pub name: Option<String>,
    /// Notes.
    #[serde(default)]
    pub body: Option<String>,
    /// Draft.
    #[serde(default)]
    pub draft: bool,
    /// Pre-release.
    #[serde(default)]
    pub prerelease: bool,
    /// Branch or commit the tag is created from.
    #[serde(default)]
    pub target_commitish: String,
    /// Author.
    #[serde(default)]
    pub author: Option<User>,
    /// Creation time.
    #[serde(default)]
    pub created_at: String,
    /// Publication time.
    #[serde(default)]
    pub published_at: Option<String>,
    /// Assets.
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A repository webhook (secrets are never returned by GitHub).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hook {
    /// Id.
    pub id: u64,
    /// Active on GitHub.
    #[serde(default)]
    pub active: bool,
    /// Subscribed events.
    #[serde(default)]
    pub events: Vec<String>,
    /// Delivery settings.
    #[serde(default)]
    pub config: HookConfig,
}

/// Webhook delivery settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookConfig {
    /// Target URL.
    #[serde(default)]
    pub url: Option<String>,
    /// `json` or `form`.
    #[serde(default)]
    pub content_type: Option<String>,
}

/// Branch protection of one branch or pattern.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protection {
    /// Required status checks.
    #[serde(default)]
    pub required_status_checks: Option<StatusChecks>,
    /// Review requirements.
    #[serde(default)]
    pub required_pull_request_reviews: Option<ReviewRules>,
    /// Admins are subject to the rules.
    #[serde(default)]
    pub enforce_admins: Option<Enabled>,
    /// Who may push.
    #[serde(default)]
    pub restrictions: Option<Restrictions>,
    /// Signed commits required.
    #[serde(default)]
    pub required_signatures: Option<Enabled>,
    /// Linear history required.
    #[serde(default)]
    pub required_linear_history: Option<Enabled>,
    /// Force pushes allowed.
    #[serde(default)]
    pub allow_force_pushes: Option<Enabled>,
    /// Deletion allowed.
    #[serde(default)]
    pub allow_deletions: Option<Enabled>,
    /// Creation of matching branches blocked.
    #[serde(default)]
    pub block_creations: Option<Enabled>,
    /// Conversations must be resolved.
    #[serde(default)]
    pub required_conversation_resolution: Option<Enabled>,
    /// Read-only branch.
    #[serde(default)]
    pub lock_branch: Option<Enabled>,
    /// Deployments that must succeed (not part of GitHub's protection object; kept for
    /// rulesets that carry it).
    #[serde(default)]
    pub required_deployments: Option<Vec<String>>,
}

/// `{"enabled": bool}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enabled {
    /// Whether the setting is on.
    #[serde(default)]
    pub enabled: bool,
}

/// Required status checks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusChecks {
    /// Branch must be up to date before merging.
    #[serde(default)]
    pub strict: bool,
    /// Check names (legacy form).
    #[serde(default)]
    pub contexts: Vec<String>,
    /// Checks with an optional app id.
    #[serde(default)]
    pub checks: Vec<CheckRef>,
}

/// A required check.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRef {
    /// Check name.
    pub context: String,
    /// GitHub App that must report it.
    #[serde(default)]
    pub app_id: Option<i64>,
}

/// Review requirements.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRules {
    /// Approvals needed.
    #[serde(default)]
    pub required_approving_review_count: u32,
    /// Code owners must approve.
    #[serde(default)]
    pub require_code_owner_reviews: bool,
    /// New pushes dismiss approvals.
    #[serde(default)]
    pub dismiss_stale_reviews: bool,
    /// Someone other than the last pusher must approve.
    #[serde(default)]
    pub require_last_push_approval: bool,
    /// Who may dismiss reviews.
    #[serde(default)]
    pub dismissal_restrictions: Option<Restrictions>,
    /// Who may bypass the review requirement.
    #[serde(default)]
    pub bypass_pull_request_allowances: Option<Restrictions>,
}

/// Users, teams and apps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restrictions {
    /// Users.
    #[serde(default)]
    pub users: Vec<User>,
    /// Teams.
    #[serde(default)]
    pub teams: Vec<Team>,
    /// Apps.
    #[serde(default)]
    pub apps: Vec<App>,
}

/// A team.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    /// Slug.
    pub slug: String,
}

/// A GitHub App.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// Slug.
    pub slug: String,
}

/// A branch from the branch list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// Name.
    pub name: String,
    /// Protected.
    #[serde(default)]
    pub protected: bool,
}

/// A force-push on a PR's head branch (GraphQL `HeadRefForcePushedEvent`).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ForcePush {
    /// When it happened.
    pub at: String,
    /// Head before (`None` when GitHub no longer knows the commit).
    pub before: Option<String>,
    /// Head after.
    pub after: Option<String>,
    /// Who pushed.
    pub actor: Option<String>,
}

/// A name-only record GitHub will not export the value of (secrets, deploy keys).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NamedItem {
    /// Kind (`actions_secret`, `deploy_key`, ...).
    pub kind: String,
    /// Name or title.
    pub name: String,
}

/// Everything fetched about one repository.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// The repository.
    pub repo: Option<Repository>,
    /// Label definitions by name.
    pub labels: BTreeMap<String, Label>,
    /// Milestones by number.
    pub milestones: BTreeMap<u64, Milestone>,
    /// Issues (not PRs) by number.
    pub issues: BTreeMap<u64, Issue>,
    /// Pull requests by number.
    pub pulls: BTreeMap<u64, PullRequest>,
    /// Reaction totals of PRs (from their issue-list entries), by number.
    pub pr_reactions: BTreeMap<u64, u64>,
    /// Protected branch names.
    pub protected_branches: BTreeSet<String>,
    /// Force pushes of each PR, in time order.
    pub force_pushes: BTreeMap<u64, Vec<ForcePush>>,
    /// Reviews of each PR by review id.
    pub reviews: BTreeMap<u64, BTreeMap<u64, Review>>,
    /// Review comments by id.
    pub review_comments: BTreeMap<u64, ReviewComment>,
    /// Conversation comments by id.
    pub issue_comments: BTreeMap<u64, IssueComment>,
    /// Per-user reactions by subject key (`issue:<n>`, `issue_comment:<id>`,
    /// `review_comment:<id>`), by reaction id.
    pub reactions: BTreeMap<String, BTreeMap<u64, Reaction>>,
    /// Releases by id.
    pub releases: BTreeMap<u64, Release>,
    /// Branch protection by branch name or pattern.
    pub protections: BTreeMap<String, Protection>,
    /// Webhooks by id.
    pub hooks: BTreeMap<u64, Hook>,
    /// Secret names and deploy keys (values are never exportable).
    pub named: BTreeSet<NamedItem>,
    /// Why a section could not be read (`hooks`, `secrets`, ...): permission problems are
    /// reported, not fatal.
    pub unreadable: BTreeMap<String, String>,
    /// Every account seen, by id.
    pub users: BTreeMap<u64, User>,
}

impl Metadata {
    /// Records an account (and the ghost for `None`) and returns it.
    pub fn see(&mut self, user: &Option<User>) -> User {
        let u = user.clone().unwrap_or_else(User::ghost);
        self.users.entry(u.id).or_insert_with(|| u.clone());
        u
    }

    /// Records every account referenced by the stored records.
    pub fn collect_users(&mut self) {
        let mut seen: Vec<Option<User>> = Vec::new();
        for i in self.issues.values() {
            seen.push(i.user.clone());
            seen.extend(i.assignees.iter().cloned().map(Some));
        }
        for p in self.pulls.values() {
            seen.push(p.user.clone());
            seen.extend(p.assignees.iter().cloned().map(Some));
            seen.extend(p.requested_reviewers.iter().cloned().map(Some));
        }
        for rs in self.reviews.values() {
            seen.extend(rs.values().map(|r| r.user.clone()));
        }
        seen.extend(self.review_comments.values().map(|c| c.user.clone()));
        seen.extend(self.issue_comments.values().map(|c| c.user.clone()));
        for rs in self.reactions.values() {
            seen.extend(rs.values().map(|r| r.user.clone()));
        }
        seen.extend(self.releases.values().map(|r| r.author.clone()));
        for p in self.protections.values() {
            if let Some(r) = &p.restrictions {
                seen.extend(r.users.iter().cloned().map(Some));
            }
        }
        for u in seen {
            self.see(&u);
        }
    }

    /// Stores an issue-list item as an issue or (for PR items) nothing: PR details come
    /// from the pulls endpoint.
    pub fn put_issue_item(&mut self, issue: Issue) {
        if issue.pull_request.is_none() {
            self.issues.insert(issue.number, issue);
        } else {
            let total = issue.reactions.as_ref().map(|r| r.total_count).unwrap_or(0);
            self.pr_reactions.insert(issue.number, total);
        }
    }

    /// The highest number used by an issue or PR.
    pub fn max_number(&self) -> u64 {
        let a = self.issues.keys().next_back().copied().unwrap_or(0);
        let b = self.pulls.keys().next_back().copied().unwrap_or(0);
        a.max(b)
    }
}

/// The trailing number of a URL path (`.../issues/12` → 12).
pub fn last_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_number_reads_the_tail() {
        assert_eq!(last_number("https://x/repos/o/r/pulls/12"), Some(12));
        assert_eq!(last_number("https://x/repos/o/r/issues/7/"), Some(7));
        assert_eq!(last_number("https://x/repos/o/r/issues"), None);
    }

    #[test]
    fn bots_are_recognized() {
        let mut u = User {
            id: 1,
            login: "dependabot[bot]".into(),
            ..User::default()
        };
        assert!(u.is_bot());
        u.login = "alice".into();
        assert!(!u.is_bot());
        u.kind = "Bot".into();
        assert!(u.is_bot());
    }

    #[test]
    fn deleted_users_become_the_ghost() {
        let mut m = Metadata::default();
        let u = m.see(&None);
        assert_eq!(u.id, GHOST_ID);
        assert!(m.users.contains_key(&GHOST_ID));
    }
}
