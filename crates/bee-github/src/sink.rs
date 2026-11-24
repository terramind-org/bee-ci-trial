//! [`ForgeSink`]: where imported metadata goes. The importer turns GitHub records into the
//! sink-neutral records below (authors already mapped to placeholder principals, text
//! already link-rewritten with the original kept, revision heads already checked against the
//! imported history) and hands them over one at a time; the sink places them and says what
//! happened to each part as [`Outcome`]s.
//!
//! Every method is an **upsert**: the same record may arrive again (a resumed import, live
//! sync, a re-run importing deltas), and placing it twice must not duplicate anything.
//! `Err` means the infrastructure failed and aborts the run (it is resumable); content the
//! target cannot take is an `Ok` outcome with a drop reason.
//!
//! Implementations: [`MemorySink`] (tests, dry runs) and [`crate::hub::HubSink`] (the
//! Beehive forge). Methods for domains a target does not have yet default to dropping the
//! record with [`reason::NOT_SUPPORTED_BY_TARGET`], so the fidelity report says so.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::links::RedirectMap;
use crate::principals::{Claim, Placeholder};
use crate::report::{Outcome, category, reason};

/// Who did something.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Author {
    /// The acting principal (placeholder, or the claimant once claimed).
    pub principal: String,
    /// GitHub numeric id.
    pub github_id: u64,
    /// GitHub login.
    pub login: String,
    /// A bot (agent principal).
    pub agent: bool,
}

/// Imported text: the body with links rewritten to Beehive, and the original.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Text {
    /// Rewritten body.
    pub body: String,
    /// The text as GitHub had it.
    pub original: String,
}

impl Text {
    /// True when rewriting changed nothing.
    pub fn unchanged(&self) -> bool {
        self.body == self.original
    }
}

/// A reaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReactionImport {
    /// GitHub reaction id.
    pub id: u64,
    /// Who.
    pub author: Author,
    /// GitHub content (`+1`, `heart`, ...).
    pub content: String,
}

/// The state of a pull request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PrState {
    /// Open.
    #[default]
    Open,
    /// Closed without merging.
    Closed,
    /// Merged on GitHub.
    Merged {
        /// The merge, squash or rebase result.
        merge_commit: Option<String>,
    },
}

/// One revision of a change request: a head the PR branch pointed at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionImport {
    /// Git commit id of the head (present in the imported history).
    pub head: String,
    /// Who pushed it.
    pub pusher: Option<Author>,
    /// When (RFC 3339; empty when unknown).
    pub at: String,
    /// The push replaced the previous head (force-push boundary).
    pub forced: bool,
}

/// A review verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    /// Approved.
    Approved,
    /// Changes requested.
    ChangesRequested,
    /// Comment only.
    #[default]
    Commented,
    /// Submitted, then dismissed.
    Dismissed,
}

/// Something that happened on a PR, in time order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ChangeEvent {
    /// A submitted review.
    Review {
        /// GitHub review id.
        id: u64,
        /// Reviewer.
        author: Author,
        /// Verdict.
        state: ReviewState,
        /// Body.
        body: Text,
        /// The head reviewed.
        commit: Option<String>,
        /// Submission time.
        at: String,
    },
    /// A review comment anchored to a revision.
    ReviewComment {
        /// GitHub comment id.
        id: u64,
        /// Author.
        author: Author,
        /// Body.
        body: Text,
        /// File path.
        path: String,
        /// `new` or `old`.
        side: String,
        /// First line of a multi-line comment.
        start_line: Option<u32>,
        /// Line (last line of a range).
        line: Option<u32>,
        /// A file-level comment.
        file_level: bool,
        /// The commit the comment was made on (`original_commit_id`).
        commit: String,
        /// GitHub's diff hunk around the anchor.
        diff_hunk: String,
        /// The comment it replies to.
        in_reply_to: Option<u64>,
        /// The review it belongs to.
        review: Option<u64>,
        /// Creation time.
        at: String,
        /// Reactions on it.
        reactions: Vec<ReactionImport>,
    },
    /// A conversation comment.
    Comment {
        /// GitHub comment id.
        id: u64,
        /// Author.
        author: Author,
        /// Body.
        body: Text,
        /// Creation time.
        at: String,
        /// Reactions on it.
        reactions: Vec<ReactionImport>,
    },
}

impl ChangeEvent {
    /// The event time (for ordering).
    pub fn at(&self) -> &str {
        match self {
            ChangeEvent::Review { at, .. }
            | ChangeEvent::ReviewComment { at, .. }
            | ChangeEvent::Comment { at, .. } => at,
        }
    }

    /// The GitHub id.
    pub fn id(&self) -> u64 {
        match self {
            ChangeEvent::Review { id, .. }
            | ChangeEvent::ReviewComment { id, .. }
            | ChangeEvent::Comment { id, .. } => *id,
        }
    }

    /// The report item name (`pull/12#review-3`, ...).
    pub fn item(&self, number: u64) -> String {
        match self {
            ChangeEvent::Review { id, .. } => format!("pull/{number}#review-{id}"),
            ChangeEvent::ReviewComment { id, .. } => format!("pull/{number}#review-comment-{id}"),
            ChangeEvent::Comment { id, .. } => format!("pull/{number}#comment-{id}"),
        }
    }
}

/// A pull request → change request with the same number.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeImport {
    /// The number (kept).
    pub number: u64,
    /// Title.
    pub title: String,
    /// Body.
    pub body: Text,
    /// Author.
    pub author: Author,
    /// State.
    pub state: PrState,
    /// Draft.
    pub draft: bool,
    /// Conversation locked.
    pub locked: bool,
    /// The bee ref the PR targets (its base branch).
    pub target_ref: String,
    /// The bee ref holding the PR's base commit (`pull/<n>/base`), used to anchor the
    /// stack of a PR whose commits already reached the target.
    pub base_ref: Option<String>,
    /// The PR this one is stacked on (its base branch is that PR's head branch).
    pub depends_on: Option<u64>,
    /// Revisions, oldest first; never empty.
    pub revisions: Vec<RevisionImport>,
    /// Label names.
    pub labels: Vec<String>,
    /// Assignees.
    pub assignees: Vec<Author>,
    /// Milestone number.
    pub milestone: Option<u64>,
    /// Pending review requests.
    pub reviewers: Vec<Author>,
    /// Reviews and comments in time order.
    pub events: Vec<ChangeEvent>,
    /// Reactions on the PR itself.
    pub reactions: Vec<ReactionImport>,
    /// Creation time.
    pub created_at: String,
    /// Close time.
    pub closed_at: Option<String>,
    /// The GitHub URL.
    pub url: String,
}

impl ChangeImport {
    /// The report item name.
    pub fn item(&self) -> String {
        format!("pull/{}", self.number)
    }
}

/// A conversation comment on an issue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentImport {
    /// GitHub id.
    pub id: u64,
    /// Author.
    pub author: Author,
    /// Body.
    pub body: Text,
    /// Creation time.
    pub at: String,
    /// Reactions.
    pub reactions: Vec<ReactionImport>,
}

/// An issue → issue record with the same number.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueImport {
    /// The number (kept).
    pub number: u64,
    /// Title.
    pub title: String,
    /// Body.
    pub body: Text,
    /// Author.
    pub author: Author,
    /// `open` or `closed`.
    pub state: String,
    /// `completed`, `not_planned`, ...
    pub state_reason: Option<String>,
    /// Label names.
    pub labels: Vec<String>,
    /// Assignees.
    pub assignees: Vec<Author>,
    /// Milestone number.
    pub milestone: Option<u64>,
    /// Locked.
    pub locked: bool,
    /// Comments in time order.
    pub comments: Vec<CommentImport>,
    /// Reactions on the issue.
    pub reactions: Vec<ReactionImport>,
    /// Creation time.
    pub created_at: String,
    /// Close time.
    pub closed_at: Option<String>,
}

/// A label definition.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelImport {
    /// Name.
    pub name: String,
    /// Color (hex, no `#`).
    pub color: String,
    /// Description.
    pub description: String,
}

/// A milestone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneImport {
    /// Number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Description.
    pub description: String,
    /// `open` or `closed`.
    pub state: String,
    /// Due date.
    pub due_on: Option<String>,
}

/// A release asset with its bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetImport {
    /// GitHub id.
    pub id: u64,
    /// File name.
    pub name: String,
    /// Label.
    pub label: String,
    /// MIME type.
    pub content_type: String,
    /// The bytes.
    #[serde(skip)]
    pub data: Vec<u8>,
}

/// A release.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseImport {
    /// GitHub id.
    pub id: u64,
    /// The tag (`tags/<tag>` in bee).
    pub tag: String,
    /// Title.
    pub name: String,
    /// Notes.
    pub body: Text,
    /// Draft.
    pub draft: bool,
    /// Pre-release.
    pub prerelease: bool,
    /// Target branch or commit.
    pub target: String,
    /// Author.
    pub author: Author,
    /// Creation time.
    pub created_at: String,
    /// Publication time.
    pub published_at: Option<String>,
    /// Assets whose download succeeded.
    pub assets: Vec<AssetImport>,
}

/// A Git LFS object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LfsImport {
    /// `sha256:<hex>`.
    pub oid: String,
    /// The bytes (verified against the oid).
    pub data: Vec<u8>,
}

/// Generated land rules for review.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyImport {
    /// `.beehive/policy.toml` text.
    pub toml: String,
    /// The CODEOWNERS path it references.
    pub codeowners: Option<String>,
}

/// A webhook to pre-fill, disabled (its secret must be re-entered).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookImport {
    /// GitHub id.
    pub id: u64,
    /// Target URL.
    pub url: String,
    /// GitHub event names.
    pub events: Vec<String>,
    /// `json` or `form`.
    pub content_type: String,
    /// It was active on GitHub.
    pub active_on_github: bool,
}

fn unsupported(cat: &str, item: String, what: &str) -> Result<Vec<Outcome>> {
    Ok(vec![Outcome::dropped(
        cat,
        item,
        reason::NOT_SUPPORTED_BY_TARGET,
        format!(
            "this Beehive server has no {what} store yet; the record is kept in the import journal and placed by a later `--resume` run"
        ),
    )])
}

/// Where imported metadata goes.
#[async_trait]
pub trait ForgeSink: Send + Sync {
    /// A short name for logs.
    fn name(&self) -> &str;

    /// Registers placeholder principals.
    async fn principals(&self, all: &[Placeholder]) -> Result<Vec<Outcome>>;

    /// Places a change request (with its revisions, reviews, comments, reactions, state).
    async fn change(&self, c: &ChangeImport) -> Result<Vec<Outcome>>;

    /// Places an issue with its comments.
    async fn issue(&self, i: &IssueImport) -> Result<Vec<Outcome>> {
        unsupported(category::ISSUES, format!("issue/{}", i.number), "issues")
    }

    /// Places a label definition.
    async fn label(&self, l: &LabelImport) -> Result<Vec<Outcome>> {
        unsupported(
            category::LABELS,
            format!("label:{}", l.name),
            "label definition",
        )
    }

    /// Places a milestone.
    async fn milestone(&self, m: &MilestoneImport) -> Result<Vec<Outcome>> {
        unsupported(
            category::MILESTONES,
            format!("milestone/{}", m.number),
            "milestone",
        )
    }

    /// Places a release with its assets.
    async fn release(&self, r: &ReleaseImport) -> Result<Vec<Outcome>> {
        let mut v = unsupported(category::RELEASES, format!("release:{}", r.tag), "release")?;
        for a in &r.assets {
            v.extend(unsupported(
                category::RELEASE_ASSETS,
                format!("release:{}/{}", r.tag, a.name),
                "release asset",
            )?);
        }
        Ok(v)
    }

    /// Stores an LFS object.
    async fn lfs_object(&self, o: &LfsImport) -> Result<Vec<Outcome>> {
        unsupported(category::LFS_OBJECTS, o.oid.clone(), "LFS object")
    }

    /// Records the generated land rules for review. `Ok(false)` when the target has no
    /// place for them (they stay in the state directory).
    async fn policy(&self, _p: &PolicyImport) -> Result<bool> {
        Ok(false)
    }

    /// Pre-fills webhooks, disabled.
    async fn webhooks(&self, hooks: &[WebhookImport]) -> Result<Vec<Outcome>> {
        let mut v = Vec::new();
        for h in hooks {
            v.extend(unsupported(
                category::WEBHOOKS,
                format!("hook/{}", h.id),
                "imported webhook",
            )?);
        }
        Ok(v)
    }

    /// Serves the redirect map. `Ok(false)` when the target has no place for it.
    async fn redirects(&self, _map: &RedirectMap) -> Result<bool> {
        Ok(false)
    }

    /// Moves everything attributed to a placeholder to the claimant; returns how many
    /// records changed.
    async fn reassign(&self, claim: &Claim) -> Result<u64>;
}

/// Everything a [`MemorySink`] holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryContents {
    /// Principals by id.
    pub principals: BTreeMap<String, Placeholder>,
    /// Change requests by number.
    pub changes: BTreeMap<u64, ChangeImport>,
    /// Issues by number.
    pub issues: BTreeMap<u64, IssueImport>,
    /// Labels by name.
    pub labels: BTreeMap<String, LabelImport>,
    /// Milestones by number.
    pub milestones: BTreeMap<u64, MilestoneImport>,
    /// Releases by tag.
    pub releases: BTreeMap<String, ReleaseImport>,
    /// LFS objects: oid → size.
    pub lfs: BTreeMap<String, u64>,
    /// The policy.
    pub policy: Option<PolicyImport>,
    /// Webhooks by GitHub id.
    pub webhooks: BTreeMap<u64, WebhookImport>,
    /// The redirect map.
    pub redirects: Option<RedirectMap>,
    /// How many times each record key was placed (to check idempotence).
    pub writes: BTreeMap<String, u64>,
}

/// An in-memory target that accepts everything (tests and dry runs). Numbers are kept by
/// construction; a number used by a PR cannot also be an issue.
#[derive(Debug, Default)]
pub struct MemorySink {
    state: Mutex<MemoryContents>,
}

impl MemorySink {
    /// An empty sink.
    pub fn new() -> MemorySink {
        MemorySink::default()
    }

    /// A copy of what it holds.
    pub fn contents(&self) -> MemoryContents {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryContents> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn change_outcomes(c: &ChangeImport) -> Vec<Outcome> {
    let mut v = vec![Outcome::imported(category::PULL_REQUESTS, c.item())];
    for (i, _) in c.revisions.iter().enumerate() {
        v.push(Outcome::imported(
            category::PR_REVISIONS,
            format!("pull/{}@r{}", c.number, i + 1),
        ));
    }
    for e in &c.events {
        let cat = match e {
            ChangeEvent::Review { .. } => category::REVIEWS,
            ChangeEvent::ReviewComment { .. } => category::REVIEW_COMMENTS,
            ChangeEvent::Comment { .. } => category::ISSUE_COMMENTS,
        };
        v.push(Outcome::imported(cat, e.item(c.number)));
        if let ChangeEvent::ReviewComment { reactions, .. }
        | ChangeEvent::Comment { reactions, .. } = e
        {
            for r in reactions {
                v.push(Outcome::imported(
                    category::REACTIONS,
                    format!("reaction/{}", r.id),
                ));
            }
        }
    }
    for r in &c.reactions {
        v.push(Outcome::imported(
            category::REACTIONS,
            format!("reaction/{}", r.id),
        ));
    }
    v
}

fn swap(a: &mut Author, claim: &Claim, n: &mut u64) {
    if a.principal == claim.placeholder {
        a.principal = claim.principal.clone();
        *n += 1;
    }
}

#[async_trait]
impl ForgeSink for MemorySink {
    fn name(&self) -> &str {
        "memory"
    }

    async fn principals(&self, all: &[Placeholder]) -> Result<Vec<Outcome>> {
        let mut s = self.lock();
        let mut v = Vec::new();
        for p in all {
            s.principals.insert(p.principal.clone(), p.clone());
            v.push(Outcome::imported(
                category::PRINCIPALS,
                format!("github-user/{}", p.github_id),
            ));
        }
        Ok(v)
    }

    async fn change(&self, c: &ChangeImport) -> Result<Vec<Outcome>> {
        let mut s = self.lock();
        if s.issues.contains_key(&c.number) {
            return Ok(vec![Outcome::dropped(
                category::PULL_REQUESTS,
                c.item(),
                reason::NUMBER_TAKEN,
                format!("#{} is already an issue", c.number),
            )]);
        }
        *s.writes.entry(c.item()).or_default() += 1;
        s.changes.insert(c.number, c.clone());
        Ok(change_outcomes(c))
    }

    async fn issue(&self, i: &IssueImport) -> Result<Vec<Outcome>> {
        let mut s = self.lock();
        let item = format!("issue/{}", i.number);
        if s.changes.contains_key(&i.number) {
            return Ok(vec![Outcome::dropped(
                category::ISSUES,
                item,
                reason::NUMBER_TAKEN,
                format!("#{} is already a change request", i.number),
            )]);
        }
        *s.writes.entry(item.clone()).or_default() += 1;
        s.issues.insert(i.number, i.clone());
        let mut v = vec![Outcome::imported(category::ISSUES, item)];
        for c in &i.comments {
            v.push(Outcome::imported(
                category::ISSUE_COMMENTS,
                format!("issue/{}#comment-{}", i.number, c.id),
            ));
            for r in &c.reactions {
                v.push(Outcome::imported(
                    category::REACTIONS,
                    format!("reaction/{}", r.id),
                ));
            }
        }
        for r in &i.reactions {
            v.push(Outcome::imported(
                category::REACTIONS,
                format!("reaction/{}", r.id),
            ));
        }
        Ok(v)
    }

    async fn label(&self, l: &LabelImport) -> Result<Vec<Outcome>> {
        self.lock().labels.insert(l.name.clone(), l.clone());
        Ok(vec![Outcome::imported(
            category::LABELS,
            format!("label:{}", l.name),
        )])
    }

    async fn milestone(&self, m: &MilestoneImport) -> Result<Vec<Outcome>> {
        self.lock().milestones.insert(m.number, m.clone());
        Ok(vec![Outcome::imported(
            category::MILESTONES,
            format!("milestone/{}", m.number),
        )])
    }

    async fn release(&self, r: &ReleaseImport) -> Result<Vec<Outcome>> {
        self.lock().releases.insert(r.tag.clone(), r.clone());
        let mut v = vec![Outcome::imported(
            category::RELEASES,
            format!("release:{}", r.tag),
        )];
        for a in &r.assets {
            v.push(Outcome::imported(
                category::RELEASE_ASSETS,
                format!("release:{}/{}", r.tag, a.name),
            ));
        }
        Ok(v)
    }

    async fn lfs_object(&self, o: &LfsImport) -> Result<Vec<Outcome>> {
        self.lock().lfs.insert(o.oid.clone(), o.data.len() as u64);
        Ok(vec![Outcome::imported(
            category::LFS_OBJECTS,
            o.oid.clone(),
        )])
    }

    async fn policy(&self, p: &PolicyImport) -> Result<bool> {
        self.lock().policy = Some(p.clone());
        Ok(true)
    }

    async fn webhooks(&self, hooks: &[WebhookImport]) -> Result<Vec<Outcome>> {
        let mut s = self.lock();
        let mut v = Vec::new();
        for h in hooks {
            s.webhooks.insert(h.id, h.clone());
            v.push(Outcome::archived(
                category::WEBHOOKS,
                format!("hook/{}", h.id),
                "pre-filled and disabled; re-enter its secret to enable it",
            ));
        }
        Ok(v)
    }

    async fn redirects(&self, map: &RedirectMap) -> Result<bool> {
        self.lock().redirects = Some(map.clone());
        Ok(true)
    }

    async fn reassign(&self, claim: &Claim) -> Result<u64> {
        let mut s = self.lock();
        let mut n = 0;
        for c in s.changes.values_mut() {
            swap(&mut c.author, claim, &mut n);
            for a in c.assignees.iter_mut().chain(c.reviewers.iter_mut()) {
                swap(a, claim, &mut n);
            }
            for r in c.reactions.iter_mut() {
                swap(&mut r.author, claim, &mut n);
            }
            for e in c.events.iter_mut() {
                match e {
                    ChangeEvent::Review { author, .. } => swap(author, claim, &mut n),
                    ChangeEvent::ReviewComment {
                        author, reactions, ..
                    }
                    | ChangeEvent::Comment {
                        author, reactions, ..
                    } => {
                        swap(author, claim, &mut n);
                        for r in reactions.iter_mut() {
                            swap(&mut r.author, claim, &mut n);
                        }
                    }
                }
            }
        }
        for i in s.issues.values_mut() {
            swap(&mut i.author, claim, &mut n);
            for a in i.assignees.iter_mut() {
                swap(a, claim, &mut n);
            }
            for c in i.comments.iter_mut() {
                swap(&mut c.author, claim, &mut n);
            }
        }
        for r in s.releases.values_mut() {
            swap(&mut r.author, claim, &mut n);
        }
        Ok(n)
    }
}
