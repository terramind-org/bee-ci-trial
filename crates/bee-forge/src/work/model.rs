//! Crate-private stored records. Wire resources (`bee.v1.Label`, `Milestone`, `Comment`, ...)
//! are stored as they are; these hold what the wire messages derive or do not carry.
//!
//! Records inherit the additive-only rule: never renumber or reuse a field.

use bee_proto::v1::{IssueFieldValue, IssueLinkedChange, Principal, ProjectItem};
use prost::Message;

/// What a number of the per-repo space is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberKind {
    /// An issue (the subject id is the issue id).
    Issue = 1,
    /// A change request (the subject id is the CR id).
    ChangeRequest = 2,
    /// A number kept only so it is never reused (a deleted issue).
    Retired = 3,
    /// A transferred issue's old number: it redirects to the new location.
    Redirect = 4,
    /// Something else that shares the space (a discussion).
    Other = 5,
}

impl NumberKind {
    /// From the stored value.
    pub fn from_i32(v: i32) -> NumberKind {
        match v {
            1 => NumberKind::Issue,
            2 => NumberKind::ChangeRequest,
            3 => NumberKind::Retired,
            4 => NumberKind::Redirect,
            _ => NumberKind::Other,
        }
    }
}

/// One entry of the shared number space.
#[derive(Clone, PartialEq, Message)]
pub struct NumberRec {
    /// [`NumberKind`].
    #[prost(int32, tag = "1")]
    pub kind: i32,
    /// Issue or change request id.
    #[prost(string, tag = "2")]
    pub subject_id: String,
    /// Redirect target repo.
    #[prost(string, tag = "3")]
    pub to_repo_id: String,
    /// Redirect target number.
    #[prost(uint64, tag = "4")]
    pub to_number: u64,
}

impl NumberRec {
    /// An entry of `kind` for `subject_id`.
    pub fn new(kind: NumberKind, subject_id: &str) -> NumberRec {
        NumberRec {
            kind: kind as i32,
            subject_id: subject_id.to_string(),
            to_repo_id: String::new(),
            to_number: 0,
        }
    }

    /// The kind.
    pub fn kind(&self) -> NumberKind {
        NumberKind::from_i32(self.kind)
    }
}

/// A reference to an issue.
#[derive(Clone, PartialEq, Message)]
pub struct RefRec {
    /// Repo id.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// Issue id (empty for a transfer origin that no longer exists).
    #[prost(string, tag = "2")]
    pub issue_id: String,
    /// Number.
    #[prost(uint64, tag = "3")]
    pub number: u64,
}

/// A reaction count.
#[derive(Clone, PartialEq, Message)]
pub struct ReactionCount {
    /// `ReactionContent`.
    #[prost(int32, tag = "1")]
    pub content: i32,
    /// How many.
    #[prost(uint32, tag = "2")]
    pub count: u32,
}

/// A stored issue.
#[derive(Clone, PartialEq, Message)]
pub struct IssueRec {
    /// Issue id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Repo id.
    #[prost(string, tag = "2")]
    pub repo_id: String,
    /// Org id.
    #[prost(string, tag = "3")]
    pub org_id: String,
    /// Number.
    #[prost(uint64, tag = "4")]
    pub number: u64,
    /// Title.
    #[prost(string, tag = "5")]
    pub title: String,
    /// Body (Markdown).
    #[prost(string, tag = "6")]
    pub body: String,
    /// `IssueState`.
    #[prost(int32, tag = "7")]
    pub state: i32,
    /// `IssueStateReason`.
    #[prost(int32, tag = "8")]
    pub state_reason: i32,
    /// The issue this one duplicates.
    #[prost(message, optional, tag = "9")]
    pub duplicate_of: Option<RefRec>,
    /// Author.
    #[prost(message, optional, tag = "10")]
    pub author: Option<Principal>,
    /// `OWNER`, `MEMBER`, ...
    #[prost(string, tag = "11")]
    pub author_association: String,
    /// Assignees.
    #[prost(message, repeated, tag = "12")]
    pub assignees: Vec<Principal>,
    /// Label ids.
    #[prost(string, repeated, tag = "13")]
    pub label_ids: Vec<String>,
    /// Milestone number, 0 = none.
    #[prost(uint64, tag = "14")]
    pub milestone_number: u64,
    /// Issue type id.
    #[prost(string, tag = "15")]
    pub issue_type_id: String,
    /// Parent issue id.
    #[prost(string, tag = "16")]
    pub parent_id: String,
    /// Direct sub-issues.
    #[prost(uint32, tag = "17")]
    pub child_count: u32,
    /// Blocked-by edges.
    #[prost(uint32, tag = "18")]
    pub blocked_by_count: u32,
    /// Blocking edges.
    #[prost(uint32, tag = "19")]
    pub blocking_count: u32,
    /// Custom field values.
    #[prost(message, repeated, tag = "20")]
    pub field_values: Vec<IssueFieldValue>,
    /// Conversation locked.
    #[prost(bool, tag = "21")]
    pub locked: bool,
    /// `IssueLockReason`.
    #[prost(int32, tag = "22")]
    pub lock_reason: i32,
    /// Pinned.
    #[prost(bool, tag = "23")]
    pub pinned: bool,
    /// Live comments.
    #[prost(uint32, tag = "24")]
    pub comment_count: u32,
    /// Reaction counts by content.
    #[prost(message, repeated, tag = "25")]
    pub reactions: Vec<ReactionCount>,
    /// All reactions.
    #[prost(uint32, tag = "26")]
    pub reaction_total: u32,
    /// Created (ms).
    #[prost(uint64, tag = "27")]
    pub created_ms: u64,
    /// Updated (ms).
    #[prost(uint64, tag = "28")]
    pub updated_ms: u64,
    /// Closed (ms), 0 while open.
    #[prost(uint64, tag = "29")]
    pub closed_ms: u64,
    /// Who closed it.
    #[prost(message, optional, tag = "30")]
    pub closed_by: Option<Principal>,
    /// Where it was transferred from.
    #[prost(message, optional, tag = "31")]
    pub transferred_from: Option<RefRec>,
    /// Linked change request ids.
    #[prost(string, repeated, tag = "32")]
    pub linked_change_ids: Vec<String>,
    /// Agent runs working the issue now.
    #[prost(string, repeated, tag = "33")]
    pub active_agent_runs: Vec<String>,
    /// Principals mentioned in the body or comments.
    #[prost(string, repeated, tag = "34")]
    pub mentioned_ids: Vec<String>,
    /// Principals who commented.
    #[prost(string, repeated, tag = "35")]
    pub commenter_ids: Vec<String>,
}

/// A development link: the wire message plus the CR's repo.
#[derive(Clone, PartialEq, Message)]
pub struct LinkRec {
    /// The link as returned.
    #[prost(message, optional, tag = "1")]
    pub change: Option<IssueLinkedChange>,
    /// Repo of the change request.
    #[prost(string, tag = "2")]
    pub repo_id: String,
}

/// A project item with the snapshot of its content the filters and views read.
#[derive(Clone, PartialEq, Message)]
pub struct ItemRec {
    /// The item as returned.
    #[prost(message, optional, tag = "1")]
    pub item: Option<ProjectItem>,
    /// Content key: `issue:<id>` or `cr:<id>`; empty for drafts.
    #[prost(string, tag = "2")]
    pub content_key: String,
    /// Label names of the content.
    #[prost(string, repeated, tag = "3")]
    pub labels: Vec<String>,
    /// Milestone title.
    #[prost(string, tag = "4")]
    pub milestone: String,
    /// Issue type name.
    #[prost(string, tag = "5")]
    pub issue_type: String,
    /// Parent issue title.
    #[prost(string, tag = "6")]
    pub parent: String,
    /// Linked change request count.
    #[prost(uint32, tag = "7")]
    pub linked_changes: u32,
    /// Repo id of the content.
    #[prost(string, tag = "8")]
    pub repo_id: String,
    /// Sub-issue progress percent.
    #[prost(uint32, tag = "9")]
    pub sub_percent: u32,
    /// Reviewer logins (change requests).
    #[prost(string, repeated, tag = "10")]
    pub reviewers: Vec<String>,
    /// Content author id.
    #[prost(string, tag = "11")]
    pub author_id: String,
}

/// One entry of a project's item activity.
#[derive(Clone, PartialEq, Message)]
pub struct ItemLogRec {
    /// Item id.
    #[prost(string, tag = "1")]
    pub item_id: String,
    /// 1 added, 2 removed (archived or deleted), 3 field set, 4 field cleared, 5 restored.
    #[prost(int32, tag = "2")]
    pub kind: i32,
    /// Field id for kinds 3 and 4.
    #[prost(string, tag = "3")]
    pub field_id: String,
    /// Display value for kind 3.
    #[prost(string, tag = "4")]
    pub display: String,
    /// Numeric value for kind 3 on number fields.
    #[prost(double, tag = "5")]
    pub number: f64,
}

/// Item activity kinds.
pub mod log_kind {
    /// The item joined the project.
    pub const ADDED: i32 = 1;
    /// The item left (archived or deleted).
    pub const REMOVED: i32 = 2;
    /// A field value was set.
    pub const SET: i32 = 3;
    /// A field value was cleared.
    pub const CLEARED: i32 = 4;
    /// An archived item came back.
    pub const RESTORED: i32 = 5;
}
