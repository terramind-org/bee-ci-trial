//! What operations report: plain data, serialized as the `data` of the CLI's `--json` output.

use serde::Serialize;

use crate::conflict;
use crate::fsutil::path_text;
use crate::manifest::Change;
use crate::state::{ConflictRecord, HiddenPath, RebaseOp, StackEntry, State};

/// One change of the stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeView {
    /// Position in the stack, from 1 at the bottom.
    pub index: usize,
    /// Stable ChangeId (`k_…`).
    pub change_id: String,
    /// Current changeset (`c1_…`).
    pub changeset: String,
    /// First parent.
    pub parent: String,
    /// First line of the message.
    pub subject: String,
    /// True if the working copy is based on this change.
    pub head: bool,
}

/// One file that differs from the base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// Repo path.
    pub path: String,
    /// `added`, `modified` or `deleted`.
    pub status: String,
    /// True if only (or also) the mode changed.
    pub mode_changed: bool,
}

/// One conflicting path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictView {
    /// Repo path.
    pub path: String,
    /// Conflict kind.
    pub kind: String,
    /// The file holds diff3 markers.
    pub markers: bool,
    /// Marked resolved.
    pub resolved: bool,
}

/// A rebase in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebaseView {
    /// `sync` or `restack`.
    pub kind: String,
    /// ChangeId of the change stopped on conflicts.
    pub current: Option<String>,
    /// Subject of that change.
    pub current_subject: Option<String>,
    /// Changes already moved.
    pub done: usize,
    /// Changes still to move.
    pub pending: usize,
}

/// A tracked path not on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenView {
    /// Repo path.
    pub path: String,
    /// Why it is not written.
    pub reason: String,
}

/// `bee status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusReport {
    /// Working copy root.
    pub root: String,
    /// Repo.
    pub repo: String,
    /// Target ref.
    pub target_ref: String,
    /// Checked-out changeset.
    pub head: String,
    /// Target ref tip the stack sits on.
    pub onto: String,
    /// Its version.
    pub onto_version: u64,
    /// The local stack, bottom first.
    pub stack: Vec<ChangeView>,
    /// Uncommitted changes against `head`, by path.
    pub changes: Vec<FileChange>,
    /// Conflicts of a rebase in progress.
    pub conflicts: Vec<ConflictView>,
    /// The rebase in progress.
    pub rebase: Option<RebaseView>,
    /// Sparse profile (`.` = whole repo).
    pub sparse: Vec<String>,
    /// Tracked paths not on disk.
    pub hidden: Vec<HiddenView>,
    /// Names on disk that are not valid repo paths.
    pub skipped: Vec<String>,
    /// No uncommitted changes and no rebase in progress.
    pub clean: bool,
    /// Operation finished by crash recovery when the working copy was opened.
    pub recovered: Option<String>,
}

/// A landed change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LandedView {
    /// ChangeId.
    pub change_id: String,
    /// Changeset that was landed.
    pub source: String,
    /// Changeset now on the ref.
    pub result: String,
    /// Already landed from the same source (an idempotent retry).
    pub skipped: bool,
}

/// What `Land` reported, coalescer outcome included.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LandView {
    /// Ref landed on.
    pub r#ref: String,
    /// Ref tip after the land.
    pub target: String,
    /// Ref version after the land.
    pub version: u64,
    /// Attempts the server made.
    pub attempts: u32,
    /// Repo event sequence of the land.
    pub event_seq: u64,
    /// Every change of the landed stack.
    pub landed: Vec<LandedView>,
}

/// Every mutating command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpReport {
    /// Operation name.
    pub op: String,
    /// Operation-log sequence number (0 when nothing changed).
    pub seq: u64,
    /// One-line summary.
    pub summary: String,
    /// Checked-out changeset afterwards.
    pub head: String,
    /// Target ref tip the stack sits on.
    pub onto: String,
    /// The stack afterwards.
    pub stack: Vec<ChangeView>,
    /// Changeset the operation created or moved to, if any.
    pub changeset: Option<String>,
    /// Its ChangeId, if any.
    pub change_id: Option<String>,
    /// Working-tree files written.
    pub written: usize,
    /// Working-tree files removed.
    pub removed: usize,
    /// Conflicts left by the operation.
    pub conflicts: Vec<ConflictView>,
    /// The rebase in progress afterwards.
    pub rebase: Option<RebaseView>,
    /// ChangeIds dropped because they became empty.
    pub dropped: Vec<String>,
    /// Tracked paths not written.
    pub hidden: Vec<HiddenView>,
    /// The land result, for `land`.
    pub land: Option<LandView>,
    /// For `undo`: the operation undone.
    pub undid: Option<String>,
    /// Newly redacted files this command replaced with the placeholder (SPEC §13).
    pub redacted: Vec<String>,
    /// Newly redacted files left alone because they hold local changes.
    pub redacted_dirty: Vec<String>,
}

/// First line of a message.
pub fn subject(message: &str) -> String {
    message.lines().next().unwrap_or_default().to_owned()
}

/// The stack of `state` as views.
pub fn stack_views(state: &State) -> Vec<ChangeView> {
    state
        .stack
        .iter()
        .enumerate()
        .map(|(i, e)| view(i, e, state.head == e.changeset))
        .collect()
}

fn view(i: usize, e: &StackEntry, head: bool) -> ChangeView {
    ChangeView {
        index: i + 1,
        change_id: e.change_id.to_text(),
        changeset: e.changeset.to_text(),
        parent: e.parent.to_text(),
        subject: subject(&e.message),
        head,
    }
}

/// Conflict views of a rebase.
pub fn conflict_views(op: Option<&RebaseOp>) -> Vec<ConflictView> {
    op.map(|op| op.conflicts.iter().map(conflict_view).collect())
        .unwrap_or_default()
}

fn conflict_view(c: &ConflictRecord) -> ConflictView {
    ConflictView {
        path: path_text(&c.path.0),
        kind: c.kind.clone(),
        markers: c.markers,
        resolved: c.resolved,
    }
}

/// The rebase view.
pub fn rebase_view(op: Option<&RebaseOp>) -> Option<RebaseView> {
    op.map(|op| RebaseView {
        kind: match op.kind {
            crate::state::RebaseKind::Sync => "sync".into(),
            crate::state::RebaseKind::Restack => "restack".into(),
        },
        current: op.current.as_ref().map(|c| c.change_id.to_text()),
        current_subject: op.current.as_ref().map(|c| subject(&c.message)),
        done: op.done.len(),
        pending: op.pending.len(),
    })
}

/// Hidden path views.
pub fn hidden_views(hidden: &[HiddenPath]) -> Vec<HiddenView> {
    hidden
        .iter()
        .map(|h| HiddenView {
            path: path_text(&h.path.0),
            reason: h.reason.clone(),
        })
        .collect()
}

/// File changes from manifest differences.
pub fn file_changes(changes: &[Change]) -> Vec<FileChange> {
    changes
        .iter()
        .map(|c| {
            let (status, mode_changed) = match (&c.old, &c.new) {
                (None, _) => ("added", false),
                (_, None) => ("deleted", false),
                (Some(o), Some(n)) => ("modified", o.mode != n.mode),
            };
            FileChange {
                path: path_text(&c.path),
                status: status.into(),
                mode_changed,
            }
        })
        .collect()
}

/// The kind name of a proto conflict kind.
pub fn kind(kind: i32) -> String {
    conflict::kind_name(kind).to_owned()
}
