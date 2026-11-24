//! Mapping Beehive events to GitHub Actions events, and deciding which workflows they trigger.
//!
//! [`map_event`] turns one Beehive event into the Actions events it produces (a push can be
//! `push` plus `create`; a change request event is both `pull_request` and
//! `pull_request_target`), each with its payload, `GITHUB_REF` and `GITHUB_SHA`.
//! [`triggers`] then decides whether one workflow runs for one Actions event, honouring
//! activity types, branch/tag/path filters, schedules, dispatch targets, `[skip ci]`, and the
//! rule that events caused by a workflow's own token start no new runs.
//! [`select_workflows`] applies that to every workflow file in a repository snapshot.

use std::collections::BTreeMap;

use crate::cron::UtcMinute;
use crate::error::WorkflowError;
use crate::model::{EventTrigger, Filter, InputType, Workflow};
use crate::parse::{default_types, parse_workflow};
use crate::value::{Object, Value, format_number};

/// Facts about the repository an event happened in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    /// Owner login.
    pub owner: String,
    /// Repository name.
    pub name: String,
    /// Default branch name, e.g. `main`.
    pub default_branch: String,
    /// Tip of the default branch.
    pub default_branch_sha: String,
    /// Web URL of the forge, e.g. `https://beehive.example`.
    pub server_url: String,
    /// REST API URL.
    pub api_url: String,
    /// GraphQL API URL.
    pub graphql_url: String,
}

impl RepoInfo {
    /// `owner/name`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// `refs/heads/<default branch>`.
    pub fn default_ref(&self) -> String {
        format!("refs/heads/{}", self.default_branch)
    }

    fn payload_repo(&self) -> Value {
        Value::object([
            ("name", Value::from(self.name.as_str())),
            ("full_name", Value::from(self.full_name())),
            ("default_branch", Value::from(self.default_branch.as_str())),
            (
                "owner",
                Value::object([("login", Value::from(self.owner.as_str()))]),
            ),
            (
                "html_url",
                Value::from(format!("{}/{}", self.server_url, self.full_name())),
            ),
        ])
    }
}

/// Changed files known for an event, for path filters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangedPaths {
    /// The list of changed files, in diff order.
    Known(Vec<String>),
    /// The diff could not be computed (timed out, or the push had more than 1,000 commits):
    /// path-filtered workflows always run.
    Unknown,
}

/// A change request (pull request) activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrAction {
    /// Opened.
    Opened,
    /// A new revision was pushed.
    Revised,
    /// Landed (merged).
    Landed,
    /// Closed without landing.
    Closed,
    /// Reopened.
    Reopened,
    /// Title or body edited.
    Edited,
    /// Marked ready for review.
    ReadyForReview,
    /// Converted to a draft.
    ConvertedToDraft,
    /// A label was added.
    Labeled,
    /// A label was removed.
    Unlabeled,
    /// An assignee was added.
    Assigned,
    /// An assignee was removed.
    Unassigned,
    /// A review was requested.
    ReviewRequested,
    /// A review request was removed.
    ReviewRequestRemoved,
    /// Conversation locked.
    Locked,
    /// Conversation unlocked.
    Unlocked,
    /// Added to the merge queue.
    Enqueued,
    /// Removed from the merge queue.
    Dequeued,
    /// Added to a milestone.
    Milestoned,
    /// Removed from a milestone.
    Demilestoned,
    /// Auto-land enabled.
    AutoMergeEnabled,
    /// Auto-land disabled.
    AutoMergeDisabled,
}

impl CrAction {
    /// The `pull_request` activity type.
    pub fn activity_type(self) -> &'static str {
        match self {
            CrAction::Opened => "opened",
            CrAction::Revised => "synchronize",
            CrAction::Landed | CrAction::Closed => "closed",
            CrAction::Reopened => "reopened",
            CrAction::Edited => "edited",
            CrAction::ReadyForReview => "ready_for_review",
            CrAction::ConvertedToDraft => "converted_to_draft",
            CrAction::Labeled => "labeled",
            CrAction::Unlabeled => "unlabeled",
            CrAction::Assigned => "assigned",
            CrAction::Unassigned => "unassigned",
            CrAction::ReviewRequested => "review_requested",
            CrAction::ReviewRequestRemoved => "review_request_removed",
            CrAction::Locked => "locked",
            CrAction::Unlocked => "unlocked",
            CrAction::Enqueued => "enqueued",
            CrAction::Dequeued => "dequeued",
            CrAction::Milestoned => "milestoned",
            CrAction::Demilestoned => "demilestoned",
            CrAction::AutoMergeEnabled => "auto_merge_enabled",
            CrAction::AutoMergeDisabled => "auto_merge_disabled",
        }
    }
}

/// A ref update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushEvent {
    /// Full ref name, `refs/heads/...` or `refs/tags/...`.
    pub git_ref: String,
    /// Old tip (all zeros when created).
    pub before: String,
    /// New tip (all zeros when deleted).
    pub after: String,
    /// The ref was created.
    pub created: bool,
    /// The ref was deleted.
    pub deleted: bool,
    /// The update was not a fast-forward.
    pub forced: bool,
    /// Number of commits pushed.
    pub commit_count: u64,
    /// Message of the new tip commit.
    pub head_commit_message: Option<String>,
    /// Files changed (two-dot diff; against the default branch for new branches).
    pub changed: ChangedPaths,
}

/// A change request (pull request) event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRequestEvent {
    /// What happened.
    pub action: CrAction,
    /// Change request number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Target branch name (`main`).
    pub base_ref: String,
    /// Target branch tip.
    pub base_sha: String,
    /// Source branch name.
    pub head_ref: String,
    /// Source tip.
    pub head_sha: String,
    /// Test-merge commit, when the change merges cleanly.
    pub merge_sha: Option<String>,
    /// The change request has merge conflicts (then `pull_request` workflows do not run).
    pub has_conflicts: bool,
    /// Draft.
    pub draft: bool,
    /// The source is a fork.
    pub from_fork: bool,
    /// Labels currently on the change request.
    pub labels: Vec<String>,
    /// The label added or removed, for label activities.
    pub label: Option<String>,
    /// Message of the head commit.
    pub head_commit_message: Option<String>,
    /// Files changed (three-dot diff against the merge base).
    pub changed: ChangedPaths,
}

/// A merge-queue entry ready for checks (`merge_group`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeQueueEvent {
    /// Target branch name.
    pub base_ref: String,
    /// Target tip.
    pub base_sha: String,
    /// The temporary queue ref (`refs/heads/gh-readonly-queue/main/pr-1-<sha>`).
    pub head_ref: String,
    /// The queue entry's commit.
    pub head_sha: String,
    /// Message of the queue commit.
    pub head_commit_message: String,
}

/// A completed or requested workflow run (`workflow_run`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowRunEvent {
    /// `requested`, `in_progress` or `completed`.
    pub action: String,
    /// The triggering workflow's display name.
    pub workflow_name: String,
    /// `success`, `failure`, ... when completed.
    pub conclusion: Option<String>,
    /// The triggering run's head branch.
    pub head_branch: String,
    /// The triggering run's head commit.
    pub head_sha: String,
    /// The triggering run's event name.
    pub event: String,
    /// The triggering run's id.
    pub run_id: u64,
    /// How many `workflow_run` links led to the triggering run (0 if it was not itself
    /// started by `workflow_run`). GitHub chains at most three levels.
    pub chain_depth: u32,
}

/// An event on Beehive.
#[derive(Clone, Debug, PartialEq)]
pub enum BeeEvent {
    /// A ref was updated.
    Push(PushEvent),
    /// A change request activity.
    ChangeRequest(ChangeRequestEvent),
    /// A review on a change request (`submitted`, `edited`, `dismissed`).
    Review {
        /// Activity type.
        action: String,
        /// The change request.
        cr: ChangeRequestEvent,
        /// Review state (`approved`, `changes_requested`, `commented`).
        state: String,
    },
    /// A merge-queue entry needs checks.
    MergeQueue(MergeQueueEvent),
    /// A release activity.
    Release {
        /// Activity type (`published`, ...).
        action: String,
        /// Tag name.
        tag: String,
        /// Tagged commit.
        sha: String,
        /// Release name.
        name: String,
        /// Pre-release flag.
        prerelease: bool,
    },
    /// An issue activity.
    Issue {
        /// Activity type.
        action: String,
        /// Issue number.
        number: u64,
        /// Title.
        title: String,
        /// Labels.
        labels: Vec<String>,
    },
    /// A comment on an issue or change request.
    IssueComment {
        /// Activity type.
        action: String,
        /// Issue or change request number.
        number: u64,
        /// Comment body.
        body: String,
        /// Whether the issue is a change request.
        on_change_request: bool,
    },
    /// The scheduler's minute tick.
    ScheduleTick(UtcMinute),
    /// A manual run of one workflow.
    WorkflowDispatch {
        /// Workflow file path.
        workflow: String,
        /// Full ref to run on.
        git_ref: String,
        /// Its tip.
        sha: String,
        /// Inputs as given (strings, as the API and UI send them).
        inputs: BTreeMap<String, String>,
    },
    /// A `repository_dispatch` API call.
    RepositoryDispatch {
        /// `event_type`.
        event_type: String,
        /// `client_payload`.
        client_payload: Value,
    },
    /// A workflow run changed state.
    WorkflowRun(WorkflowRunEvent),
    /// Any other webhook-style event (`label`, `watch`, `fork`, `discussion`, ...).
    Other {
        /// Actions event name.
        name: String,
        /// Activity type, if the event has types.
        action: Option<String>,
        /// Payload fields besides `action`, `repository` and `sender`.
        payload: Value,
    },
}

/// Who caused an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cause {
    /// Login of the actor.
    pub actor: String,
    /// The event was caused by a workflow run's own token (`GITHUB_TOKEN`); such events start
    /// no workflows except `workflow_dispatch` and `repository_dispatch`.
    pub by_workflow_token: bool,
}

/// What a workflow's filters are matched against.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterSubject {
    /// No filters apply.
    None,
    /// A branch push (name without `refs/heads/`) with its changed files.
    Branch(String, ChangedPaths),
    /// A tag push (name without `refs/tags/`); path filters are not evaluated.
    Tag(String),
    /// A pull request: base branch and changed files.
    PullRequest(String, ChangedPaths),
    /// A merge group: base branch.
    MergeGroup(String),
    /// A workflow run: workflow name and head branch.
    WorkflowRun(String, String, u32),
    /// A schedule tick.
    Schedule(UtcMinute),
    /// A dispatch of one workflow file, with its inputs.
    Dispatch(String, BTreeMap<String, String>),
}

/// One GitHub Actions event produced by a Beehive event.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionsEvent {
    /// `github.event_name`.
    pub name: String,
    /// The activity type (`github.event.action`), when the event has one.
    pub action: Option<String>,
    /// `GITHUB_REF`.
    pub git_ref: String,
    /// `GITHUB_SHA`.
    pub sha: String,
    /// `GITHUB_BASE_REF` (pull requests).
    pub base_ref: String,
    /// `GITHUB_HEAD_REF` (pull requests).
    pub head_ref: String,
    /// `github.event`.
    pub payload: Value,
    /// `github.actor`.
    pub actor: String,
    /// What filters match against.
    pub subject: FilterSubject,
    /// The head commit message asks to skip CI (`push` and `pull_request` only).
    pub skip_ci: bool,
    /// Caused by a workflow token.
    pub by_workflow_token: bool,
}

/// Strings in a commit message that skip `push` and `pull_request` workflows.
pub const SKIP_CI_MARKERS: [&str; 5] = [
    "[skip ci]",
    "[ci skip]",
    "[no ci]",
    "[skip actions]",
    "[actions skip]",
];

/// Whether a commit message skips CI: a marker anywhere, or a `skip-checks: true` trailer.
pub fn wants_skip_ci(message: &str) -> bool {
    if SKIP_CI_MARKERS.iter().any(|m| message.contains(m)) {
        return true;
    }
    let trailers = message.trim_end().rsplit("\n\n").next().unwrap_or_default();
    message.contains("\n\n")
        && trailers
            .lines()
            .any(|l| matches!(l.trim(), "skip-checks:true" | "skip-checks: true"))
}

fn short_ref(git_ref: &str) -> String {
    git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref)
        .to_owned()
}

fn with_common(payload: Vec<(&str, Value)>, repo: &RepoInfo, actor: &str) -> Value {
    let mut o = Object::new();
    for (k, v) in payload {
        o.insert(k, v);
    }
    o.insert("repository", repo.payload_repo());
    o.insert("sender", Value::object([("login", Value::from(actor))]));
    Value::from(o)
}

fn cr_payload(cr: &ChangeRequestEvent, repo: &RepoInfo) -> Value {
    let merged = cr.action == CrAction::Landed;
    let closed = matches!(cr.action, CrAction::Landed | CrAction::Closed);
    Value::object([
        ("number", Value::Number(cr.number as f64)),
        (
            "html_url",
            Value::from(format!(
                "{}/{}/pull/{}",
                repo.server_url,
                repo.full_name(),
                cr.number
            )),
        ),
        ("title", Value::from(cr.title.as_str())),
        ("state", Value::from(if closed { "closed" } else { "open" })),
        ("draft", Value::Bool(cr.draft)),
        ("merged", Value::Bool(merged)),
        (
            "merge_commit_sha",
            cr.merge_sha.as_deref().map_or(Value::Null, Value::from),
        ),
        (
            "labels",
            Value::array(
                cr.labels
                    .iter()
                    .map(|l| Value::object([("name", Value::from(l.as_str()))])),
            ),
        ),
        (
            "head",
            Value::object([
                ("ref", Value::from(cr.head_ref.as_str())),
                ("sha", Value::from(cr.head_sha.as_str())),
                (
                    "repo",
                    Value::object([
                        ("fork", Value::Bool(cr.from_fork)),
                        ("full_name", Value::from(repo.full_name())),
                    ]),
                ),
            ]),
        ),
        (
            "base",
            Value::object([
                ("ref", Value::from(cr.base_ref.as_str())),
                ("sha", Value::from(cr.base_sha.as_str())),
            ]),
        ),
    ])
}

/// The Actions events a Beehive event produces, with payloads, refs and SHAs (GitHub's
/// "Events that trigger workflows" table).
pub fn map_event(event: &BeeEvent, cause: &Cause, repo: &RepoInfo) -> Vec<ActionsEvent> {
    let actor = cause.actor.as_str();
    let base = |name: &str,
                action: Option<&str>,
                git_ref: String,
                sha: String,
                payload: Value,
                subject: FilterSubject| ActionsEvent {
        name: name.to_owned(),
        action: action.map(str::to_owned),
        git_ref,
        sha,
        base_ref: String::new(),
        head_ref: String::new(),
        payload,
        actor: actor.to_owned(),
        subject,
        skip_ci: false,
        by_workflow_token: cause.by_workflow_token,
    };
    let default_ref = repo.default_ref();
    let default_sha = repo.default_branch_sha.clone();
    let mut out = Vec::new();
    match event {
        BeeEvent::Push(p) => {
            let is_tag = p.git_ref.starts_with("refs/tags/");
            let name = short_ref(&p.git_ref);
            let sha = if p.deleted {
                default_sha.clone()
            } else {
                p.after.clone()
            };
            let head_commit = match &p.head_commit_message {
                Some(m) if !p.deleted => Value::object([
                    ("id", Value::from(p.after.as_str())),
                    ("message", Value::from(m.as_str())),
                ]),
                _ => Value::Null,
            };
            let payload = with_common(
                vec![
                    ("ref", Value::from(p.git_ref.as_str())),
                    ("before", Value::from(p.before.as_str())),
                    ("after", Value::from(p.after.as_str())),
                    ("created", Value::Bool(p.created)),
                    ("deleted", Value::Bool(p.deleted)),
                    ("forced", Value::Bool(p.forced)),
                    ("head_commit", head_commit),
                    ("pusher", Value::object([("name", Value::from(actor))])),
                ],
                repo,
                actor,
            );
            let changed = if p.commit_count > 1000 {
                ChangedPaths::Unknown
            } else {
                p.changed.clone()
            };
            let subject = if is_tag {
                FilterSubject::Tag(name.clone())
            } else {
                FilterSubject::Branch(name.clone(), changed)
            };
            let mut push = base("push", None, p.git_ref.clone(), sha, payload, subject);
            push.skip_ci = p.head_commit_message.as_deref().is_some_and(wants_skip_ci);
            out.push(push);
            let ref_type = if is_tag { "tag" } else { "branch" };
            if p.created {
                let payload = with_common(
                    vec![
                        ("ref", Value::from(name.as_str())),
                        ("ref_type", Value::from(ref_type)),
                        ("master_branch", Value::from(repo.default_branch.as_str())),
                    ],
                    repo,
                    actor,
                );
                out.push(base(
                    "create",
                    None,
                    p.git_ref.clone(),
                    p.after.clone(),
                    payload,
                    FilterSubject::None,
                ));
            }
            if p.deleted {
                let payload = with_common(
                    vec![
                        ("ref", Value::from(name.as_str())),
                        ("ref_type", Value::from(ref_type)),
                    ],
                    repo,
                    actor,
                );
                out.push(base(
                    "delete",
                    None,
                    default_ref.clone(),
                    default_sha.clone(),
                    payload,
                    FilterSubject::None,
                ));
            }
        }
        BeeEvent::ChangeRequest(cr) => {
            let action = cr.action.activity_type();
            let mut fields = vec![
                ("action", Value::from(action)),
                ("number", Value::Number(cr.number as f64)),
                ("pull_request", cr_payload(cr, repo)),
            ];
            if let Some(l) = &cr.label {
                fields.push(("label", Value::object([("name", Value::from(l.as_str()))])));
            }
            let payload = with_common(fields, repo, actor);
            if !cr.has_conflicts {
                let sha = cr.merge_sha.clone().unwrap_or_else(|| cr.head_sha.clone());
                let mut e = base(
                    "pull_request",
                    Some(action),
                    format!("refs/pull/{}/merge", cr.number),
                    sha,
                    payload.clone(),
                    FilterSubject::PullRequest(cr.base_ref.clone(), cr.changed.clone()),
                );
                e.base_ref = cr.base_ref.clone();
                e.head_ref = cr.head_ref.clone();
                e.skip_ci = cr.head_commit_message.as_deref().is_some_and(wants_skip_ci);
                out.push(e);
            }
            let mut t = base(
                "pull_request_target",
                Some(action),
                default_ref.clone(),
                default_sha.clone(),
                payload,
                FilterSubject::PullRequest(cr.base_ref.clone(), cr.changed.clone()),
            );
            t.base_ref = cr.base_ref.clone();
            t.head_ref = cr.head_ref.clone();
            out.push(t);
        }
        BeeEvent::Review { action, cr, state } => {
            let payload = with_common(
                vec![
                    ("action", Value::from(action.as_str())),
                    (
                        "review",
                        Value::object([("state", Value::from(state.as_str()))]),
                    ),
                    ("pull_request", cr_payload(cr, repo)),
                ],
                repo,
                actor,
            );
            let mut e = base(
                "pull_request_review",
                Some(action),
                format!("refs/pull/{}/merge", cr.number),
                cr.merge_sha.clone().unwrap_or_else(|| cr.head_sha.clone()),
                payload,
                FilterSubject::None,
            );
            e.base_ref = cr.base_ref.clone();
            e.head_ref = cr.head_ref.clone();
            out.push(e);
        }
        BeeEvent::MergeQueue(mq) => {
            let payload = with_common(
                vec![
                    ("action", Value::from("checks_requested")),
                    (
                        "merge_group",
                        Value::object([
                            ("head_sha", Value::from(mq.head_sha.as_str())),
                            ("head_ref", Value::from(mq.head_ref.as_str())),
                            ("base_sha", Value::from(mq.base_sha.as_str())),
                            (
                                "base_ref",
                                Value::from(format!("refs/heads/{}", mq.base_ref)),
                            ),
                            (
                                "head_commit",
                                Value::object([
                                    ("id", Value::from(mq.head_sha.as_str())),
                                    ("message", Value::from(mq.head_commit_message.as_str())),
                                ]),
                            ),
                        ]),
                    ),
                ],
                repo,
                actor,
            );
            out.push(base(
                "merge_group",
                Some("checks_requested"),
                mq.head_ref.clone(),
                mq.head_sha.clone(),
                payload,
                FilterSubject::MergeGroup(mq.base_ref.clone()),
            ));
        }
        BeeEvent::Release {
            action,
            tag,
            sha,
            name,
            prerelease,
        } => {
            let payload = with_common(
                vec![
                    ("action", Value::from(action.as_str())),
                    (
                        "release",
                        Value::object([
                            ("tag_name", Value::from(tag.as_str())),
                            ("name", Value::from(name.as_str())),
                            ("prerelease", Value::Bool(*prerelease)),
                            ("target_commitish", Value::from(sha.as_str())),
                        ]),
                    ),
                ],
                repo,
                actor,
            );
            out.push(base(
                "release",
                Some(action),
                format!("refs/tags/{tag}"),
                sha.clone(),
                payload,
                FilterSubject::None,
            ));
        }
        BeeEvent::Issue {
            action,
            number,
            title,
            labels,
        } => {
            let payload =
                with_common(
                    vec![
                        ("action", Value::from(action.as_str())),
                        (
                            "issue",
                            Value::object([
                                ("number", Value::Number(*number as f64)),
                                (
                                    "html_url",
                                    Value::from(format!(
                                        "{}/{}/issues/{number}",
                                        repo.server_url,
                                        repo.full_name()
                                    )),
                                ),
                                ("title", Value::from(title.as_str())),
                                (
                                    "labels",
                                    Value::array(labels.iter().map(|l| {
                                        Value::object([("name", Value::from(l.as_str()))])
                                    })),
                                ),
                            ]),
                        ),
                    ],
                    repo,
                    actor,
                );
            out.push(base(
                "issues",
                Some(action),
                default_ref,
                default_sha,
                payload,
                FilterSubject::None,
            ));
        }
        BeeEvent::IssueComment {
            action,
            number,
            body,
            on_change_request,
        } => {
            let mut issue = Object::new();
            issue.insert("number", Value::Number(*number as f64));
            if *on_change_request {
                issue.insert(
                    "pull_request",
                    Value::object([(
                        "url",
                        Value::from(format!(
                            "{}/repos/{}/pulls/{number}",
                            repo.api_url,
                            repo.full_name()
                        )),
                    )]),
                );
            }
            let payload = with_common(
                vec![
                    ("action", Value::from(action.as_str())),
                    ("issue", Value::from(issue)),
                    (
                        "comment",
                        Value::object([("body", Value::from(body.as_str()))]),
                    ),
                ],
                repo,
                actor,
            );
            out.push(base(
                "issue_comment",
                Some(action),
                default_ref,
                default_sha,
                payload,
                FilterSubject::None,
            ));
        }
        BeeEvent::ScheduleTick(t) => {
            let payload = with_common(vec![], repo, actor);
            out.push(base(
                "schedule",
                None,
                default_ref,
                default_sha,
                payload,
                FilterSubject::Schedule(*t),
            ));
        }
        BeeEvent::WorkflowDispatch {
            workflow,
            git_ref,
            sha,
            inputs,
        } => {
            let payload = with_common(
                vec![
                    ("ref", Value::from(git_ref.as_str())),
                    ("workflow", Value::from(workflow.as_str())),
                    (
                        "inputs",
                        Value::from(
                            inputs
                                .iter()
                                .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                                .collect::<Object>(),
                        ),
                    ),
                ],
                repo,
                actor,
            );
            out.push(base(
                "workflow_dispatch",
                None,
                git_ref.clone(),
                sha.clone(),
                payload,
                FilterSubject::Dispatch(workflow.clone(), inputs.clone()),
            ));
        }
        BeeEvent::RepositoryDispatch {
            event_type,
            client_payload,
        } => {
            let payload = with_common(
                vec![
                    ("action", Value::from(event_type.as_str())),
                    ("branch", Value::from(repo.default_branch.as_str())),
                    ("client_payload", client_payload.clone()),
                ],
                repo,
                actor,
            );
            out.push(base(
                "repository_dispatch",
                Some(event_type),
                default_ref,
                default_sha,
                payload,
                FilterSubject::None,
            ));
        }
        BeeEvent::WorkflowRun(r) => {
            let payload = with_common(
                vec![
                    ("action", Value::from(r.action.as_str())),
                    (
                        "workflow_run",
                        Value::object([
                            ("id", Value::Number(r.run_id as f64)),
                            ("name", Value::from(r.workflow_name.as_str())),
                            ("head_branch", Value::from(r.head_branch.as_str())),
                            ("head_sha", Value::from(r.head_sha.as_str())),
                            ("event", Value::from(r.event.as_str())),
                            (
                                "conclusion",
                                r.conclusion.as_deref().map_or(Value::Null, Value::from),
                            ),
                        ]),
                    ),
                ],
                repo,
                actor,
            );
            out.push(base(
                "workflow_run",
                Some(&r.action),
                default_ref,
                default_sha,
                payload,
                FilterSubject::WorkflowRun(
                    r.workflow_name.clone(),
                    r.head_branch.clone(),
                    r.chain_depth,
                ),
            ));
        }
        BeeEvent::Other {
            name,
            action,
            payload,
        } => {
            let mut fields: Vec<(&str, Value)> = Vec::new();
            if let Some(a) = action {
                fields.push(("action", Value::from(a.as_str())));
            }
            let mut o = payload.as_object().cloned().unwrap_or_default();
            for (k, v) in fields {
                o.insert(k, v);
            }
            o.insert("repository", repo.payload_repo());
            o.insert("sender", Value::object([("login", Value::from(actor))]));
            out.push(base(
                name,
                action.as_deref(),
                default_ref,
                default_sha,
                Value::from(o),
                FilterSubject::None,
            ));
        }
    }
    out
}

/// GitHub's diff-size limit for path filters: matches beyond this many files are not seen.
pub const MAX_FILTERED_PATHS: usize = 3000;

/// Why a workflow does not run for an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The workflow does not listen to this event.
    NoTrigger,
    /// Caused by a workflow token.
    WorkflowToken,
    /// The activity type is not selected.
    Type,
    /// A branch or tag filter excluded the ref.
    Ref,
    /// A path filter excluded every changed file (or none changed).
    Paths,
    /// `[skip ci]` or a `skip-checks` trailer.
    SkipCi,
    /// No cron schedule matches this minute.
    Schedule,
    /// A dispatch for another workflow.
    OtherWorkflow,
    /// `workflow_run` chains are limited to three levels.
    ChainTooDeep,
}

/// A decision that a workflow runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Triggered {
    /// The matching cron expression, for `schedule` (one run per matching entry).
    pub schedule: Option<String>,
}

fn paths_allow(filter: &Filter, changed: &ChangedPaths) -> bool {
    let files = match changed {
        ChangedPaths::Unknown => return true,
        ChangedPaths::Known(f) => f,
    };
    if files.is_empty() {
        return false;
    }
    let files = &files[..files.len().min(MAX_FILTERED_PATHS)];
    match filter {
        Filter::Include(p) => files.iter().any(|f| p.matches(f)),
        Filter::Ignore(p) => !files.iter().all(|f| p.matches(f)),
    }
}

fn types_allow(t: &EventTrigger, action: Option<&str>) -> bool {
    let Some(action) = action else {
        return true;
    };
    match &t.types {
        Some(list) => list.iter().any(|x| x == action),
        None => default_types(&t.name).is_none_or(|d| d.contains(&action)),
    }
}

/// Decides whether `wf` runs for `ev`. A `schedule` event can match several cron entries;
/// each is one run.
pub fn triggers(wf: &Workflow, ev: &ActionsEvent) -> Result<Vec<Triggered>, Skip> {
    let Some(t) = wf.trigger(&ev.name) else {
        return Err(Skip::NoTrigger);
    };
    if ev.by_workflow_token
        && !matches!(
            ev.name.as_str(),
            "workflow_dispatch" | "repository_dispatch"
        )
    {
        return Err(Skip::WorkflowToken);
    }
    if !types_allow(t, ev.action.as_deref()) {
        return Err(Skip::Type);
    }
    if ev.skip_ci && matches!(ev.name.as_str(), "push" | "pull_request") {
        return Err(Skip::SkipCi);
    }
    let once = Ok(vec![Triggered { schedule: None }]);
    match &ev.subject {
        FilterSubject::None => once,
        FilterSubject::Branch(branch, changed) => {
            let has_ref_filter = t.branches.is_some() || t.tags.is_some();
            match &t.branches {
                Some(f) if !f.allows(branch) => return Err(Skip::Ref),
                None if has_ref_filter => return Err(Skip::Ref),
                _ => {}
            }
            if let Some(p) = &t.paths
                && !paths_allow(p, changed)
            {
                return Err(Skip::Paths);
            }
            once
        }
        FilterSubject::Tag(tag) => {
            let has_ref_filter = t.branches.is_some() || t.tags.is_some();
            match &t.tags {
                Some(f) if !f.allows(tag) => Err(Skip::Ref),
                None if has_ref_filter => Err(Skip::Ref),
                _ => once,
            }
        }
        FilterSubject::PullRequest(base, changed) => {
            if let Some(f) = &t.branches
                && !f.allows(base)
            {
                return Err(Skip::Ref);
            }
            if let Some(p) = &t.paths
                && !paths_allow(p, changed)
            {
                return Err(Skip::Paths);
            }
            once
        }
        FilterSubject::MergeGroup(base) => match &t.branches {
            Some(f) if !f.allows(base) => Err(Skip::Ref),
            _ => once,
        },
        FilterSubject::WorkflowRun(name, branch, depth) => {
            if *depth >= 3 {
                return Err(Skip::ChainTooDeep);
            }
            if let Some(w) = &t.workflows
                && !w.matches(name)
            {
                return Err(Skip::NoTrigger);
            }
            match &t.branches {
                Some(f) if !f.allows(branch) => Err(Skip::Ref),
                _ => once,
            }
        }
        FilterSubject::Schedule(minute) => {
            let hits: Vec<Triggered> = t
                .schedules
                .iter()
                .filter(|c| c.matches(minute))
                .map(|c| Triggered {
                    schedule: Some(c.source().to_owned()),
                })
                .collect();
            if hits.is_empty() {
                Err(Skip::Schedule)
            } else {
                Ok(hits)
            }
        }
        FilterSubject::Dispatch(path, _) => {
            if path == &wf.path {
                once
            } else {
                Err(Skip::OtherWorkflow)
            }
        }
    }
}

/// Validates `workflow_dispatch` inputs against the declarations and returns the `inputs`
/// context (typed: booleans and numbers) and the `github.event.inputs` object (strings), both
/// with defaults applied.
pub fn dispatch_inputs(
    wf: &Workflow,
    given: &BTreeMap<String, String>,
) -> Result<(Object, Object), String> {
    let decls = wf
        .trigger("workflow_dispatch")
        .map(|t| t.inputs.as_slice())
        .unwrap_or_default();
    for k in given.keys() {
        if !decls.iter().any(|d| &d.id == k) {
            return Err(format!("Unexpected inputs provided: [\"{k}\"]"));
        }
    }
    let mut typed = Object::new();
    let mut strings = Object::new();
    for d in decls {
        let raw = match given.get(&d.id) {
            Some(v) => Some(v.clone()),
            None => d.default.as_ref().and_then(|x| match x {
                crate::model::Dyn::Str(t, _) => Some(t.render()),
                crate::model::Dyn::Number(n, _) => Some(format_number(*n)),
                crate::model::Dyn::Bool(b, _) => Some(b.to_string()),
                _ => None,
            }),
        };
        let Some(raw) = raw else {
            if d.required {
                return Err(format!("Required input '{}' not provided", d.id));
            }
            let empty = match d.kind {
                InputType::Boolean => Value::Bool(false),
                _ => Value::from(""),
            };
            typed.insert(d.id.clone(), empty);
            continue;
        };
        if d.required && raw.is_empty() && d.kind != InputType::Boolean {
            return Err(format!("Required input '{}' not provided", d.id));
        }
        let v = match d.kind {
            InputType::Boolean => match raw.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => {
                    return Err(format!(
                        "Provided value '{raw}' for input '{}' is not a boolean",
                        d.id
                    ));
                }
            },
            InputType::Number => {
                let n = crate::value::parse_number(&raw);
                if n.is_nan() || raw.trim().is_empty() {
                    return Err(format!(
                        "Provided value '{raw}' for input '{}' is not a number",
                        d.id
                    ));
                }
                Value::Number(n)
            }
            InputType::Choice => {
                if !d.options.contains(&raw) {
                    return Err(format!(
                        "Provided value '{raw}' for input '{}' not in the list of allowed values",
                        d.id
                    ));
                }
                Value::from(raw.as_str())
            }
            InputType::String | InputType::Environment => Value::from(raw.as_str()),
        };
        typed.insert(d.id.clone(), v);
        strings.insert(d.id.clone(), Value::from(raw));
    }
    Ok((typed, strings))
}

/// Whether a path is a workflow file GitHub reads: a `.yml` or `.yaml` file directly in
/// `.github/workflows`.
pub fn is_workflow_path(path: &str) -> bool {
    path.strip_prefix(".github/workflows/")
        .is_some_and(|f| !f.contains('/') && (f.ends_with(".yml") || f.ends_with(".yaml")))
}

/// The outcome of checking one workflow file against an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    /// The workflow runs (once per entry).
    Run(Box<Workflow>, Vec<Triggered>),
    /// The workflow file is invalid. GitHub reports such files as failed runs.
    Invalid(WorkflowError),
}

/// Checks every workflow file in a snapshot against an event, in path order. Files that do
/// not trigger are left out.
pub fn select_workflows(
    files: &BTreeMap<String, String>,
    ev: &ActionsEvent,
) -> Vec<(String, Selection)> {
    let mut out = Vec::new();
    for (path, source) in files {
        if !is_workflow_path(path) {
            continue;
        }
        match parse_workflow(path, source) {
            Ok(wf) => {
                if let Ok(t) = triggers(&wf, ev) {
                    out.push((path.clone(), Selection::Run(Box::new(wf), t)));
                }
            }
            Err(e) => out.push((path.clone(), Selection::Invalid(e))),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn repo() -> RepoInfo {
        RepoInfo {
            owner: "octo".into(),
            name: "app".into(),
            default_branch: "main".into(),
            default_branch_sha: "d".repeat(40),
            server_url: "https://beehive.test".into(),
            api_url: "https://beehive.test/api/v3".into(),
            graphql_url: "https://beehive.test/api/graphql".into(),
        }
    }

    fn cause() -> Cause {
        Cause {
            actor: "mona".into(),
            by_workflow_token: false,
        }
    }

    fn wf(on: &str) -> Workflow {
        parse_workflow(
            ".github/workflows/w.yml",
            &format!("{on}\njobs:\n  a:\n    runs-on: x\n    steps: [{{run: x}}]\n"),
        )
        .unwrap()
    }

    fn push(git_ref: &str, files: &[&str]) -> PushEvent {
        PushEvent {
            git_ref: git_ref.into(),
            before: "a".repeat(40),
            after: "b".repeat(40),
            created: false,
            deleted: false,
            forced: false,
            commit_count: 1,
            head_commit_message: Some("fix things".into()),
            changed: ChangedPaths::Known(files.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn runs(w: &Workflow, e: &BeeEvent) -> bool {
        map_event(e, &cause(), &repo())
            .iter()
            .any(|ev| triggers(w, ev).is_ok())
    }

    fn cr(action: CrAction) -> ChangeRequestEvent {
        ChangeRequestEvent {
            action,
            number: 7,
            title: "Add x".into(),
            base_ref: "main".into(),
            base_sha: "c".repeat(40),
            head_ref: "feature".into(),
            head_sha: "e".repeat(40),
            merge_sha: Some("f".repeat(40)),
            has_conflicts: false,
            draft: false,
            from_fork: false,
            labels: vec![],
            label: None,
            head_commit_message: None,
            changed: ChangedPaths::Known(vec!["src/lib.rs".into()]),
        }
    }

    #[test]
    fn push_branch_and_tag_filters() {
        // GitHub's example: branches main, mona/octocat, releases/**; tags v2, v1.*
        let w = wf(
            "on:\n  push:\n    branches: [main, 'mona/octocat', 'releases/**']\n    tags: [v2, v1.*]",
        );
        assert!(runs(&w, &BeeEvent::Push(push("refs/heads/main", &["a"]))));
        assert!(runs(
            &w,
            &BeeEvent::Push(push("refs/heads/releases/10", &["a"]))
        ));
        assert!(runs(&w, &BeeEvent::Push(push("refs/tags/v1.9.1", &[]))));
        assert!(!runs(&w, &BeeEvent::Push(push("refs/heads/dev", &["a"]))));
        assert!(!runs(&w, &BeeEvent::Push(push("refs/tags/v3", &[]))));
        // Only branches defined: tag pushes do not run.
        let w = wf("on:\n  push:\n    branches: ['**']");
        assert!(!runs(&w, &BeeEvent::Push(push("refs/tags/v1", &[]))));
        // Only tags defined: branch pushes do not run.
        let w = wf("on:\n  push:\n    tags: ['v*']");
        assert!(!runs(&w, &BeeEvent::Push(push("refs/heads/main", &["a"]))));
        // Neither: both run.
        let w = wf("on: push");
        assert!(runs(&w, &BeeEvent::Push(push("refs/tags/v1", &[]))));
        // Ignore lists (GitHub's branches-ignore/tags-ignore example).
        let w = wf(
            "on:\n  push:\n    branches-ignore: ['mona/octocat', 'releases/**-alpha']\n    tags-ignore: [v2, v1.*]",
        );
        assert!(!runs(
            &w,
            &BeeEvent::Push(push("refs/heads/releases/beta/3-alpha", &["a"]))
        ));
        assert!(runs(
            &w,
            &BeeEvent::Push(push("refs/heads/releases/beta/3", &["a"]))
        ));
        assert!(!runs(&w, &BeeEvent::Push(push("refs/tags/v1.9", &[]))));
    }

    #[test]
    fn path_filters() {
        let w = wf("on:\n  push:\n    paths: ['sub-project/**', '!sub-project/docs/**']");
        assert!(runs(
            &w,
            &BeeEvent::Push(push("refs/heads/main", &["sub-project/src/index.js"]))
        ));
        assert!(!runs(
            &w,
            &BeeEvent::Push(push("refs/heads/main", &["sub-project/docs/readme.md"]))
        ));
        let w = wf("on:\n  push:\n    paths-ignore: ['docs/**']");
        assert!(!runs(
            &w,
            &BeeEvent::Push(push("refs/heads/main", &["docs/a.md", "docs/b.md"]))
        ));
        assert!(runs(
            &w,
            &BeeEvent::Push(push("refs/heads/main", &["docs/a.md", "src/x.rs"]))
        ));
        // No changed files: path-filtered workflows do not run.
        assert!(!runs(&w, &BeeEvent::Push(push("refs/heads/main", &[]))));
        // Path filters are not evaluated for tag pushes.
        let w = wf("on:\n  push:\n    paths: ['src/**']");
        assert!(runs(&w, &BeeEvent::Push(push("refs/tags/v1", &[]))));
        // More than 1,000 commits: always runs.
        let mut p = push("refs/heads/main", &["docs/a"]);
        p.commit_count = 1001;
        assert!(runs(&w, &BeeEvent::Push(p)));
        // Matches beyond the first 3,000 files are not seen.
        let mut files: Vec<String> = (0..3000).map(|i| format!("docs/{i}")).collect();
        files.push("src/late.rs".into());
        let mut p = push("refs/heads/main", &[]);
        p.changed = ChangedPaths::Known(files);
        assert!(!runs(&w, &BeeEvent::Push(p)));
    }

    #[test]
    fn pull_request_types_and_mapping() {
        let w = wf("on: pull_request");
        assert!(runs(&w, &BeeEvent::ChangeRequest(cr(CrAction::Opened))));
        assert!(runs(&w, &BeeEvent::ChangeRequest(cr(CrAction::Revised))));
        assert!(runs(&w, &BeeEvent::ChangeRequest(cr(CrAction::Reopened))));
        assert!(!runs(&w, &BeeEvent::ChangeRequest(cr(CrAction::Landed))));
        assert!(!runs(&w, &BeeEvent::ChangeRequest(cr(CrAction::Labeled))));
        let w = wf("on:\n  pull_request:\n    types: [closed]\n    branches: [main]");
        let evs = map_event(
            &BeeEvent::ChangeRequest(cr(CrAction::Landed)),
            &cause(),
            &repo(),
        );
        let pr = &evs[0];
        assert_eq!(pr.name, "pull_request");
        assert_eq!(pr.git_ref, "refs/pull/7/merge");
        assert_eq!(pr.sha, "f".repeat(40));
        assert_eq!(
            pr.payload.get("pull_request").unwrap().get("merged"),
            Some(&Value::Bool(true))
        );
        assert!(triggers(&w, pr).is_ok());
        let mut other = cr(CrAction::Landed);
        other.base_ref = "dev".into();
        assert!(!runs(&w, &BeeEvent::ChangeRequest(other)));
        // pull_request_target runs in the base context.
        let t = &evs[1];
        assert_eq!(t.name, "pull_request_target");
        assert_eq!(t.git_ref, "refs/heads/main");
        assert_eq!(t.sha, "d".repeat(40));
    }

    #[test]
    fn merge_conflicts_block_pull_request_only() {
        let mut c = cr(CrAction::Revised);
        c.has_conflicts = true;
        let evs = map_event(&BeeEvent::ChangeRequest(c), &cause(), &repo());
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].name, "pull_request_target");
    }

    #[test]
    fn skip_ci_markers() {
        assert!(wants_skip_ci("Fix [skip ci]"));
        assert!(wants_skip_ci("docs\n\n[ci skip]"));
        assert!(wants_skip_ci("Tweak\n\n\nskip-checks: true"));
        assert!(!wants_skip_ci("skip-checks: true"));
        assert!(!wants_skip_ci("Fix ci"));
        let w = wf("on: [push, pull_request_target]");
        let mut p = push("refs/heads/main", &["a"]);
        p.head_commit_message = Some("wip [no ci]".into());
        assert!(!runs(&w, &BeeEvent::Push(p)));
        let mut c = cr(CrAction::Opened);
        c.head_commit_message = Some("[skip actions]".into());
        // pull_request_target is not affected by skip markers.
        assert!(runs(&w, &BeeEvent::ChangeRequest(c)));
    }

    #[test]
    fn create_and_delete_events() {
        let mut p = push("refs/heads/feat", &["a"]);
        p.created = true;
        let evs = map_event(&BeeEvent::Push(p), &cause(), &repo());
        assert_eq!(
            evs.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["push", "create"]
        );
        assert_eq!(evs[1].payload.get("ref_type"), Some(&Value::from("branch")));
        let mut p = push("refs/heads/feat", &[]);
        p.deleted = true;
        let evs = map_event(&BeeEvent::Push(p), &cause(), &repo());
        assert_eq!(
            evs[0].sha,
            "d".repeat(40),
            "a deleted branch's run uses the default branch tip"
        );
        assert_eq!(evs[1].name, "delete");
        assert_eq!(evs[1].git_ref, "refs/heads/main");
    }

    #[test]
    fn workflow_token_events_do_not_trigger_except_dispatch() {
        let w = wf("on: [push, repository_dispatch]");
        let c = Cause {
            actor: "github-actions[bot]".into(),
            by_workflow_token: true,
        };
        let evs = map_event(
            &BeeEvent::Push(push("refs/heads/main", &["a"])),
            &c,
            &repo(),
        );
        assert_eq!(triggers(&w, &evs[0]), Err(Skip::WorkflowToken));
        let evs = map_event(
            &BeeEvent::RepositoryDispatch {
                event_type: "x".into(),
                client_payload: Value::Null,
            },
            &c,
            &repo(),
        );
        assert!(triggers(&w, &evs[0]).is_ok());
    }

    #[test]
    fn schedule_matching_yields_one_run_per_cron() {
        let w = wf(
            "on:\n  schedule:\n    - cron: '0 * * * *'\n    - cron: '0 0 * * *'\n    - cron: '30 1 * * *'",
        );
        let evs = map_event(
            &BeeEvent::ScheduleTick(UtcMinute {
                year: 2026,
                month: 10,
                day: 18,
                hour: 0,
                minute: 0,
            }),
            &cause(),
            &repo(),
        );
        let t = triggers(&w, &evs[0]).unwrap();
        assert_eq!(
            t.iter()
                .map(|x| x.schedule.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["0 * * * *", "0 0 * * *"]
        );
        let evs = map_event(
            &BeeEvent::ScheduleTick(UtcMinute {
                year: 2026,
                month: 10,
                day: 18,
                hour: 0,
                minute: 5,
            }),
            &cause(),
            &repo(),
        );
        assert_eq!(triggers(&w, &evs[0]), Err(Skip::Schedule));
        assert_eq!(evs[0].git_ref, "refs/heads/main");
    }

    #[test]
    fn dispatch_targets_one_workflow_and_validates_inputs() {
        let w = wf(
            "on:\n  workflow_dispatch:\n    inputs:\n      level:\n        type: choice\n        options: [info, debug]\n        default: info\n      dry:\n        type: boolean\n        default: true\n      n:\n        type: number\n      who:\n        required: true",
        );
        let mut inputs = BTreeMap::new();
        inputs.insert("who".to_owned(), "mona".to_owned());
        let e = BeeEvent::WorkflowDispatch {
            workflow: w.path.clone(),
            git_ref: "refs/heads/main".into(),
            sha: "1".repeat(40),
            inputs: inputs.clone(),
        };
        assert!(runs(&w, &e));
        let other = BeeEvent::WorkflowDispatch {
            workflow: ".github/workflows/x.yml".into(),
            git_ref: "refs/heads/main".into(),
            sha: "1".repeat(40),
            inputs: inputs.clone(),
        };
        assert!(!runs(&w, &other));
        let (typed, strings) = dispatch_inputs(&w, &inputs).unwrap();
        assert_eq!(typed.get("dry"), Some(&Value::Bool(true)));
        assert_eq!(strings.get("dry"), Some(&Value::from("true")));
        assert_eq!(typed.get("level"), Some(&Value::from("info")));
        assert_eq!(typed.get("n"), Some(&Value::from("")));
        assert!(
            dispatch_inputs(&w, &BTreeMap::new())
                .unwrap_err()
                .contains("Required input 'who'")
        );
        let mut bad = inputs.clone();
        bad.insert("level".into(), "trace".into());
        assert!(dispatch_inputs(&w, &bad).is_err());
        let mut bad = inputs.clone();
        bad.insert("dry".into(), "yes".into());
        assert!(dispatch_inputs(&w, &bad).is_err());
        let mut bad = inputs;
        bad.insert("extra".into(), "1".into());
        assert!(
            dispatch_inputs(&w, &bad)
                .unwrap_err()
                .contains("Unexpected inputs")
        );
    }

    #[test]
    fn repository_dispatch_types() {
        let w = wf("on:\n  repository_dispatch:\n    types: [deploy]");
        let e = |t: &str| BeeEvent::RepositoryDispatch {
            event_type: t.into(),
            client_payload: Value::object([("env", Value::from("prod"))]),
        };
        assert!(runs(&w, &e("deploy")));
        assert!(!runs(&w, &e("test")));
    }

    #[test]
    fn workflow_run_filters_and_chain_limit() {
        let w = wf(
            "on:\n  workflow_run:\n    workflows: [CI, 'Build *']\n    types: [completed]\n    branches: [main]",
        );
        let mk = |name: &str, action: &str, branch: &str, depth: u32| {
            BeeEvent::WorkflowRun(WorkflowRunEvent {
                action: action.into(),
                workflow_name: name.into(),
                conclusion: Some("success".into()),
                head_branch: branch.into(),
                head_sha: "1".repeat(40),
                event: "push".into(),
                run_id: 9,
                chain_depth: depth,
            })
        };
        assert!(runs(&w, &mk("CI", "completed", "main", 0)));
        assert!(runs(&w, &mk("Build linux", "completed", "main", 2)));
        assert!(!runs(&w, &mk("CI", "requested", "main", 0)));
        assert!(!runs(&w, &mk("Lint", "completed", "main", 0)));
        assert!(!runs(&w, &mk("CI", "completed", "dev", 0)));
        assert!(!runs(&w, &mk("CI", "completed", "main", 3)));
    }

    #[test]
    fn merge_group_release_issues_and_other_events() {
        let w = wf(
            "on:\n  merge_group:\n  release:\n    types: [published]\n  issues:\n    types: [opened, labeled]\n  issue_comment:\n  watch:\n    types: [started]",
        );
        let mq = BeeEvent::MergeQueue(MergeQueueEvent {
            base_ref: "main".into(),
            base_sha: "1".repeat(40),
            head_ref: "refs/heads/gh-readonly-queue/main/pr-7-abc".into(),
            head_sha: "2".repeat(40),
            head_commit_message: "Merge".into(),
        });
        let evs = map_event(&mq, &cause(), &repo());
        assert_eq!(evs[0].git_ref, "refs/heads/gh-readonly-queue/main/pr-7-abc");
        assert!(runs(&w, &mq));
        let rel = |a: &str| BeeEvent::Release {
            action: a.into(),
            tag: "v1.0.0".into(),
            sha: "3".repeat(40),
            name: "v1".into(),
            prerelease: false,
        };
        assert!(runs(&w, &rel("published")));
        assert!(!runs(&w, &rel("created")));
        assert_eq!(
            map_event(&rel("published"), &cause(), &repo())[0].git_ref,
            "refs/tags/v1.0.0"
        );
        let issue = |a: &str| BeeEvent::Issue {
            action: a.into(),
            number: 1,
            title: "t".into(),
            labels: vec![],
        };
        assert!(runs(&w, &issue("labeled")));
        assert!(!runs(&w, &issue("closed")));
        assert!(runs(
            &w,
            &BeeEvent::IssueComment {
                action: "created".into(),
                number: 1,
                body: "/deploy".into(),
                on_change_request: true
            }
        ));
        assert!(runs(
            &w,
            &BeeEvent::Other {
                name: "watch".into(),
                action: Some("started".into()),
                payload: Value::Null
            }
        ));
        assert!(!runs(
            &w,
            &BeeEvent::Other {
                name: "fork".into(),
                action: None,
                payload: Value::Null
            }
        ));
    }

    #[test]
    fn selection_over_a_snapshot() {
        let mut files = BTreeMap::new();
        files.insert(
            ".github/workflows/ci.yml".to_owned(),
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: x}]\n".to_owned(),
        );
        files.insert(
            ".github/workflows/pr.yaml".to_owned(),
            "on: pull_request\njobs:\n  a:\n    runs-on: x\n    steps: [{run: x}]\n".to_owned(),
        );
        files.insert(
            ".github/workflows/bad.yml".to_owned(),
            "on: push\njobs: 3\n".to_owned(),
        );
        files.insert(
            ".github/workflows/sub/nested.yml".to_owned(),
            "on: push\n".to_owned(),
        );
        files.insert(".github/workflows/notes.txt".to_owned(), "x".to_owned());
        let evs = map_event(
            &BeeEvent::Push(push("refs/heads/main", &["a"])),
            &cause(),
            &repo(),
        );
        let sel = select_workflows(&files, &evs[0]);
        let paths: Vec<&str> = sel.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec![".github/workflows/bad.yml", ".github/workflows/ci.yml"]
        );
        assert!(matches!(sel[0].1, Selection::Invalid(_)));
    }
}
