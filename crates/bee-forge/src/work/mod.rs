//! Issues, labels, milestones and projects (SPEC §11.8, PRODUCT §4.15). Serves
//! `IssueService` and `ProjectService`; the design is in `docs/spec/forge-work.md`.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | tables, migrations, event kinds, the [`Work`] store handle, hooks, numbers |
//! | `model.rs` | crate-private stored records |
//! | `text.rs` | Markdown task lists, closing keywords, mentions and references |
//! | `issues.rs` | issues, comments, reactions, timeline, locking, pinning, transfer |
//! | `labels.rs` | labels and milestones |
//! | `fields.rs` | org issue types and typed custom issue fields |
//! | `relations.rs` | sub-issues, dependencies, development links |
//! | `assign.rs` | assignees and agent runs |
//! | `templates.rs` | issue templates and YAML issue forms |
//! | `saved.rs` | saved replies |
//! | `search.rs` | issue list filters evaluated over the store |
//! | `projects.rs`, `items.rs`, `views.rs`, `automation.rs`, `insights.rs` | Projects v2 |
//! | `listener.rs` | reacts to change-request events |
//!
//! Every method follows `docs/spec/forge-core.md` §2.3: one transaction on the `work` surface,
//! `&Cx` first, optimistic versions, `tx.emit_event` and `tx.audit` for every change. Issue and
//! project events carry the resource as proto3-JSON in `payload_json` with a `subject`.

/// Proto3-JSON of a message, empty on failure.
macro_rules! pjson {
    ($m:expr) => {
        serde_json::to_string($m).unwrap_or_default()
    };
}

mod assign;
mod automation;
mod fields;
mod insights;
mod issues;
mod items;
mod labels;
pub mod listener;
pub mod model;
pub(crate) mod pfilter;
pub mod projects;
mod relations;
mod saved;
mod search;
mod templates;
pub mod text;
mod views;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_proto::v1::{Event, IssueRef, Principal, Repo, RepoRef, SubjectKind, SubjectRef, repo_ref};

use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::module::{EventKind, ForgeModule};
use crate::store::{
    Cursor, Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef, Tx,
};

pub use assign::{AgentAssignment, is_agent};
pub use automation::{ChangeTrigger, WorkflowPatch, WorkflowSpec};
pub use issues::{
    ImportedComment, ImportedIssue, IssuePage, IssueTarget, NewIssue, ResolvedRef, TextEffects,
    TimelinePage,
};
/// An issue record by id, wherever it lives now (the posts module lists adoptions by their
/// issues' current repo, number and title).
pub(crate) async fn load_issue_by_id(
    tx: &mut Tx,
    id: &str,
) -> Result<Option<(model::IssueRec, Row)>> {
    issues::load_issue_id(tx, id, false).await
}
pub use items::{ItemContent, ItemQuery};
pub use listener::{NamedIssue, WorkListener};
pub use model::NumberKind;
pub use pfilter::EvalCtx;
pub use projects::{CopyRequest, OwnerInfo, ProjectListQuery};
pub use relations::{ChangeLink, default_branch_name};
pub use search::{IssueListQuery, ListSort, StateSelector};
pub use templates::{
    NoTemplates, TemplateFile, TemplateFiles, TemplateSource, parse_template, render_form_body,
};
pub use views::{ViewPatch, ViewSpec};

/// The work surface.
pub const SURFACE: Surface = Surface("work");

/// Marks a repo full name the store could not fill in because the repo is not the one the
/// call was about. The service replaces `#id:<repo id>` with the real name.
pub const NAME_PLACEHOLDER: &str = "#id:";

macro_rules! table {
    ($(#[$m:meta])* $name:ident, $phys:literal, uk1 $uk1:literal, uk2 $uk2:literal, [$($idx:expr),*]) => {
        $(#[$m])*
        pub static $name: TableDef = TableDef {
            name: $phys,
            surface: SURFACE,
            unique_k1: $uk1,
            unique_k2: $uk2,
            indexes: &[$($idx),*],
        };
    };
}

table!(
    /// Issues by repo; `k1` number (zero padded, unique), `k2` org id, `n1` updated (ms).
    ISSUES, "forge_work_issues", uk1 true, uk2 false, [Index::ScopeK1, Index::ScopeN1, Index::K2]
);
table!(
    /// The per-repo number space shared with change requests: `id` is the zero-padded number,
    /// `k1` the subject id.
    NUMBERS, "forge_work_numbers", uk1 false, uk2 false, [Index::K1]
);
table!(
    /// Counters: scope is the owner of the sequence, `id` its name, `n1` the last value.
    COUNTERS, "forge_work_counters", uk1 false, uk2 false, []
);
table!(
    /// Object id to owning scope: scope is the kind, `k1` the scope key of the object.
    IDS, "forge_work_ids", uk1 false, uk2 false, []
);
table!(
    /// Issue comments by issue; `k1` author, `n1` created (ms).
    COMMENTS, "forge_work_comments", uk1 false, uk2 false, [Index::ScopeN1]
);
table!(
    /// Edit history of comments, by comment.
    COMMENT_EDITS, "forge_work_comment_edits", uk1 false, uk2 false, []
);
table!(
    /// Reactions by subject (issue or comment); id is `<principal>:<content>`.
    REACTIONS, "forge_work_reactions", uk1 false, uk2 false, [Index::ScopeK1]
);
table!(
    /// Issue timeline by issue; `k1` event kind.
    TIMELINE, "forge_work_timeline", uk1 false, uk2 false, [Index::ScopeK1]
);
table!(
    /// Labels by repo; `k1` lower-cased name (unique).
    LABELS, "forge_work_labels", uk1 true, uk2 false, []
);
table!(
    /// Milestones by repo; `k1` lower-cased title (unique), `k2` number (unique).
    MILESTONES, "forge_work_milestones", uk1 true, uk2 true, [Index::ScopeK2]
);
table!(
    /// Issue types by org; `k1` lower-cased name (unique).
    TYPES, "forge_work_types", uk1 true, uk2 false, []
);
table!(
    /// Custom issue fields by org; `k1` lower-cased name (unique).
    FIELDS, "forge_work_fields", uk1 true, uk2 false, []
);
table!(
    /// Sub-issues by parent issue; `n1` position.
    SUBS, "forge_work_subs", uk1 false, uk2 false, [Index::ScopeN1]
);
table!(
    /// Dependency edges by issue; id is `by:<issue>` or `ing:<issue>`.
    DEPS, "forge_work_deps", uk1 false, uk2 false, []
);
table!(
    /// Linked change requests by issue; `k1` change request id.
    LINKS, "forge_work_links", uk1 false, uk2 false, [Index::K1]
);
table!(
    /// Branches created for an issue, by issue.
    BRANCHES, "forge_work_branches", uk1 false, uk2 false, []
);
table!(
    /// Pinned issues by repo; `n1` pin order.
    PINS, "forge_work_pins", uk1 false, uk2 false, [Index::ScopeN1]
);
table!(
    /// Saved replies by user; `k1` lower-cased title.
    SAVED, "forge_work_saved_replies", uk1 false, uk2 false, []
);
table!(
    /// Projects by owner; `k1` number (zero padded, unique), `n1` created (ms).
    PROJECTS, "forge_work_projects", uk1 true, uk2 false, [Index::ScopeK1]
);
table!(
    /// Repos linked to projects: scope is the repo, id the project, `k1` the owner.
    PROJECT_REPOS, "forge_work_project_repos", uk1 false, uk2 false, []
);
table!(
    /// Project fields by project; `k1` lower-cased name (unique).
    PROJECT_FIELDS, "forge_work_project_fields", uk1 true, uk2 false, []
);
table!(
    /// Project items by project; `k1` content key (unique), `n1` position.
    PROJECT_ITEMS, "forge_work_project_items", uk1 true, uk2 false, [Index::ScopeN1]
);
table!(
    /// Items by content: scope is the content key (`issue:<id>`, `cr:<id>`), id the item id,
    /// `k1` the project id.
    ITEM_INDEX, "forge_work_item_index", uk1 false, uk2 false, [Index::K1]
);
table!(
    /// Project views by project; `k1` number (unique).
    VIEWS, "forge_work_project_views", uk1 true, uk2 false, []
);
table!(
    /// Project workflows by project.
    WORKFLOWS, "forge_work_project_workflows", uk1 false, uk2 false, []
);
table!(
    /// Project charts by project.
    CHARTS, "forge_work_project_charts", uk1 false, uk2 false, []
);
table!(
    /// Project status updates by project.
    STATUS_UPDATES, "forge_work_project_status", uk1 false, uk2 false, []
);
table!(
    /// Item activity by project, for historical charts; `k1` item id, `n1` time (ms).
    ITEM_LOG, "forge_work_item_log", uk1 false, uk2 false, [Index::ScopeN1]
);

static M1: Migration = Migration {
    module: "work",
    version: 1,
    name: "create_issues_and_projects",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&ISSUES),
        MigrationOp::CreateTable(&NUMBERS),
        MigrationOp::CreateTable(&COUNTERS),
        MigrationOp::CreateTable(&IDS),
        MigrationOp::CreateTable(&COMMENTS),
        MigrationOp::CreateTable(&COMMENT_EDITS),
        MigrationOp::CreateTable(&REACTIONS),
        MigrationOp::CreateTable(&TIMELINE),
        MigrationOp::CreateTable(&LABELS),
        MigrationOp::CreateTable(&MILESTONES),
        MigrationOp::CreateTable(&TYPES),
        MigrationOp::CreateTable(&FIELDS),
        MigrationOp::CreateTable(&SUBS),
        MigrationOp::CreateTable(&DEPS),
        MigrationOp::CreateTable(&LINKS),
        MigrationOp::CreateTable(&BRANCHES),
        MigrationOp::CreateTable(&PINS),
        MigrationOp::CreateTable(&SAVED),
        MigrationOp::CreateTable(&PROJECTS),
        MigrationOp::CreateTable(&PROJECT_REPOS),
        MigrationOp::CreateTable(&PROJECT_FIELDS),
        MigrationOp::CreateTable(&PROJECT_ITEMS),
        MigrationOp::CreateTable(&ITEM_INDEX),
        MigrationOp::CreateTable(&VIEWS),
        MigrationOp::CreateTable(&WORKFLOWS),
        MigrationOp::CreateTable(&CHARTS),
        MigrationOp::CreateTable(&STATUS_UPDATES),
        MigrationOp::CreateTable(&ITEM_LOG),
    ],
};

/// What the work module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "work",
    surfaces: &[SURFACE],
    // Issue adoptions of posts (`crate::posts::adopt`) are written by the issue's own
    // transaction, so their table lives on this surface.
    migrations: &[&M1, &crate::posts::M_WORK],
    event_kinds: &[
        EventKind::gh("issue.opened", "An issue was opened", "issues", "opened"),
        EventKind::gh("issue.closed", "An issue was closed", "issues", "closed"),
        EventKind::gh(
            "issue.reopened",
            "An issue was reopened",
            "issues",
            "reopened",
        ),
        EventKind::gh("issue.edited", "An issue was edited", "issues", "edited"),
        EventKind::gh("issue.deleted", "An issue was deleted", "issues", "deleted"),
        EventKind::gh(
            "issue.labeled",
            "Labels were added or removed",
            "issues",
            "labeled",
        ),
        EventKind::gh(
            "issue.assigned",
            "Assignees were added",
            "issues",
            "assigned",
        ),
        EventKind::gh(
            "issue.unassigned",
            "Assignees were removed",
            "issues",
            "unassigned",
        ),
        EventKind::gh(
            "issue.milestoned",
            "A milestone was set or cleared",
            "issues",
            "milestoned",
        ),
        EventKind::gh(
            "issue.locked",
            "A conversation was locked",
            "issues",
            "locked",
        ),
        EventKind::gh(
            "issue.unlocked",
            "A conversation was unlocked",
            "issues",
            "unlocked",
        ),
        EventKind::gh("issue.pinned", "An issue was pinned", "issues", "pinned"),
        EventKind::gh(
            "issue.unpinned",
            "An issue was unpinned",
            "issues",
            "unpinned",
        ),
        EventKind::gh(
            "issue.transferred",
            "An issue moved repos",
            "issues",
            "transferred",
        ),
        EventKind::gh("issue.typed", "An issue's type changed", "issues", "typed"),
        EventKind::new(
            "issue.assigned_to_agent",
            "An agent principal was assigned an issue; the agent runner consumes this",
        ),
        EventKind::new(
            "issue.agent_run_finished",
            "An agent run on an issue finished",
        ),
        EventKind::new(
            "issue.duplicate_marked",
            "An issue was marked as a duplicate",
        ),
        EventKind::new("issue.duplicate_unmarked", "A duplicate mark was cleared"),
        EventKind::new("issue.field_changed", "A custom field value changed"),
        EventKind::new(
            "issue.sub_issue_changed",
            "A sub-issue was added, removed or moved",
        ),
        EventKind::new(
            "issue.dependency_changed",
            "A blocked-by edge was added or removed",
        ),
        EventKind::new(
            "issue.change_linked",
            "A change request was linked or unlinked",
        ),
        EventKind::new("issue.branch_created", "A branch was created for an issue"),
        EventKind::new("issue.referenced", "An issue was referenced from elsewhere"),
        EventKind::gh(
            "issue_comment.created",
            "A comment was added to an issue",
            "issue_comment",
            "created",
        ),
        EventKind::gh(
            "issue_comment.edited",
            "An issue comment was edited",
            "issue_comment",
            "edited",
        ),
        EventKind::gh(
            "issue_comment.deleted",
            "An issue comment was deleted",
            "issue_comment",
            "deleted",
        ),
        EventKind::new(
            "issue_comment.reacted",
            "A reaction changed on an issue or comment",
        ),
        EventKind::gh("label.created", "A label was created", "label", "created"),
        EventKind::gh("label.edited", "A label was edited", "label", "edited"),
        EventKind::gh("label.deleted", "A label was deleted", "label", "deleted"),
        EventKind::gh(
            "milestone.created",
            "A milestone was created",
            "milestone",
            "created",
        ),
        EventKind::gh(
            "milestone.edited",
            "A milestone was edited",
            "milestone",
            "edited",
        ),
        EventKind::gh(
            "milestone.closed",
            "A milestone was closed",
            "milestone",
            "closed",
        ),
        EventKind::gh(
            "milestone.opened",
            "A milestone was reopened",
            "milestone",
            "opened",
        ),
        EventKind::gh(
            "milestone.deleted",
            "A milestone was deleted",
            "milestone",
            "deleted",
        ),
        EventKind::new(
            "issue_type.changed",
            "An org issue type was created, edited or deleted",
        ),
        EventKind::new(
            "issue_field.changed",
            "An org issue field was created, edited or deleted",
        ),
        EventKind::new("saved_reply.changed", "A saved reply changed"),
        EventKind::gh(
            "project.created",
            "A project was created",
            "projects_v2",
            "created",
        ),
        EventKind::gh(
            "project.edited",
            "A project was edited",
            "projects_v2",
            "edited",
        ),
        EventKind::gh(
            "project.closed",
            "A project was closed",
            "projects_v2",
            "closed",
        ),
        EventKind::gh(
            "project.deleted",
            "A project was deleted",
            "projects_v2",
            "deleted",
        ),
        EventKind::gh(
            "project_item.changed",
            "A project item was added, edited, moved, archived or deleted",
            "projects_v2_item",
            "edited",
        ),
        EventKind::gh(
            "project_status_update.changed",
            "A project status update changed",
            "projects_v2_status_update",
            "edited",
        ),
        EventKind::new(
            "project_config.changed",
            "A project's fields, views, workflows or charts changed",
        ),
    ],
    audit_prefixes: &[
        "issue.",
        "issue_comment.",
        "issue_type.",
        "issue_field.",
        "milestone.",
        "label.",
        "saved_reply.",
        "project.",
        "project_item.",
        "project_config.",
        "project_status_update.",
    ],
};

/// Largest number of rows read for one list or fan-out inside a transaction.
pub const SCAN_CAP: usize = 50_000;
/// Rows fetched per scan round trip.
const CHUNK: usize = 1_000;

/// The repo a call is about, as the service resolved it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoInfo {
    /// Repo id.
    pub id: String,
    /// Owning org id.
    pub org_id: String,
    /// `org/name`.
    pub full_name: String,
}

impl From<&Repo> for RepoInfo {
    fn from(r: &Repo) -> RepoInfo {
        RepoInfo {
            id: r.id.clone(),
            org_id: r.org_id.clone(),
            full_name: r.full_name.clone(),
        }
    }
}

/// Who acts, with the standing the service computed for them on the repo.
#[derive(Clone, Debug, Default)]
pub struct Actor {
    /// The acting principal (kind and display filled from the directory).
    pub principal: Principal,
    /// May triage: edit anyone's issue, comment on locked issues, manage labels.
    pub triage: bool,
    /// Repo admin: delete issues and comments.
    pub admin: bool,
    /// `OWNER`, `MEMBER`, `COLLABORATOR`, `CONTRIBUTOR` or `NONE`.
    pub association: String,
}

impl Actor {
    /// An actor with full standing, for tests and background work.
    pub fn staff(principal: Principal) -> Actor {
        Actor {
            principal,
            triage: true,
            admin: true,
            association: "OWNER".into(),
        }
    }
}

/// Hooks the server backs; every one has a default so the module works without them.
#[derive(Clone)]
pub struct WorkHooks {
    /// Reads issue templates from the default ref.
    pub templates: Arc<dyn TemplateSource>,
    /// Creates branches and workspaces for issues.
    pub branches: Arc<dyn BranchProvisioner>,
    /// Decides whether an agent takes work and has budget.
    pub agents: Arc<dyn AgentGate>,
    /// Reads change requests for project items.
    pub changes: Arc<dyn ChangeSource>,
}

impl Default for WorkHooks {
    fn default() -> WorkHooks {
        WorkHooks {
            templates: Arc::new(templates::NoTemplates),
            branches: Arc::new(DetachedBranches),
            agents: Arc::new(AllAgents),
            changes: Arc::new(NoChanges),
        }
    }
}

/// Creates the VCS side of an issue branch.
#[async_trait]
pub trait BranchProvisioner: Send + Sync {
    /// Creates `ref_name` from `base_ref` in `repo`; `ALREADY_EXISTS` when it exists. Returns
    /// the workspace id when `workspace` is set.
    async fn create_branch(
        &self,
        repo: &Repo,
        ref_name: &str,
        base_ref: &str,
        workspace: bool,
    ) -> Result<String>;
}

/// Answers `UNIMPLEMENTED` / `served_elsewhere`: no VCS is attached.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedBranches;

#[async_trait]
impl BranchProvisioner for DetachedBranches {
    async fn create_branch(&self, _: &Repo, _: &str, _: &str, _: bool) -> Result<String> {
        Err(ForgeError::unimplemented(
            "CreateIssueBranch needs the VCS; owner: server wiring (BranchProvisioner)",
        )
        .with_subcode("served_elsewhere"))
    }
}

/// Decides whether assigning an agent starts a run.
#[async_trait]
pub trait AgentGate: Send + Sync {
    /// `Ok(true)` when `agent` is configured to take work in `repo`, `Ok(false)` when it is
    /// only a bystander, `BUDGET_EXHAUSTED` when its run cannot start for lack of budget.
    async fn admit(&self, repo: &Repo, agent: &Principal) -> Result<bool>;
}

/// Every agent takes work.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllAgents;

#[async_trait]
impl AgentGate for AllAgents {
    async fn admit(&self, _: &Repo, _: &Principal) -> Result<bool> {
        Ok(true)
    }
}

/// What the work module needs to know about a change request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeInfo {
    /// Change request id.
    pub id: String,
    /// Per-repo number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// `open`, `landed`, `closed` or `draft`.
    pub state: String,
    /// Assignees.
    pub assignees: Vec<Principal>,
}

/// Reads change requests (the changes module backs it).
#[async_trait]
pub trait ChangeSource: Send + Sync {
    /// The change request `id_or_number` of `repo`, or `None` when unknown.
    async fn change(&self, repo: &Repo, id: &str) -> Result<Option<ChangeInfo>>;
}

/// No change requests are known: project items for them start from the number registry.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoChanges;

#[async_trait]
impl ChangeSource for NoChanges {
    async fn change(&self, _: &Repo, _: &str) -> Result<Option<ChangeInfo>> {
        Ok(None)
    }
}

/// The work store: issues, planning objects and projects over the `work` surface.
pub struct Work {
    db: Db,
    hooks: RwLock<WorkHooks>,
    /// The posts module, once installed (`crate::posts::Posts::attach`): issues name the posts
    /// that inspired them.
    posts: std::sync::OnceLock<Arc<crate::posts::Posts>>,
}

impl Work {
    /// A work store over `db` with default hooks.
    pub fn new(db: Db) -> Work {
        Work {
            db,
            hooks: RwLock::new(WorkHooks::default()),
            posts: std::sync::OnceLock::new(),
        }
    }

    /// Installs the posts module (once).
    pub fn set_posts(&self, posts: Arc<crate::posts::Posts>) {
        let _ = self.posts.set(posts);
    }

    /// The installed posts module, if any.
    pub fn posts(&self) -> Option<&Arc<crate::posts::Posts>> {
        self.posts.get()
    }

    /// Replaces the hooks.
    pub fn set_hooks(&self, hooks: WorkHooks) {
        if let Ok(mut h) = self.hooks.write() {
            *h = hooks;
        }
    }

    /// The current hooks.
    pub fn hooks(&self) -> WorkHooks {
        self.hooks.read().map(|h| h.clone()).unwrap_or_default()
    }

    /// The store handle.
    pub fn db(&self) -> &Db {
        &self.db
    }

    // ---- the per-repo number space (SPEC §11.8, §17.3) ----

    /// Allocates the next number of `repo_id` for `subject_id`. Change requests call this
    /// before creating the CR so issues and CRs share one number space; the number is never
    /// reused, even when the caller's own transaction fails afterwards.
    pub async fn allocate_number(
        &self,
        cx: &Cx,
        repo_id: &str,
        kind: NumberKind,
        subject_id: &str,
    ) -> Result<u64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let n = allocate(&mut tx, repo_id, kind, subject_id).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Claims a specific `number` (imports keep GitHub's numbers). The counter moves past it;
    /// `ALREADY_EXISTS` when the number is taken.
    pub async fn reserve_number(
        &self,
        cx: &Cx,
        repo_id: &str,
        number: u64,
        kind: NumberKind,
        subject_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        reserve(&mut tx, repo_id, number, kind, subject_id).await?;
        tx.commit().await
    }

    /// What `number` is in `repo_id`, if anything.
    pub async fn number_entry(
        &self,
        cx: &Cx,
        repo_id: &str,
        number: u64,
    ) -> Result<Option<model::NumberRec>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<model::NumberRec>(&NUMBERS, repo_id, &pad(number))
            .await?
            .map(|(r, _)| r))
    }
}

// ---- shared helpers ----

/// Zero-padded decimal, so text keys sort numerically.
pub(crate) fn pad(n: u64) -> String {
    format!("{n:012}")
}

/// Lower-cased trimmed name used as a unique key.
pub(crate) fn lc(s: &str) -> String {
    s.trim().to_lowercase()
}

/// A scan cursor after the row with `id` in `scope`.
pub(crate) fn cursor(scope: &str, after: &str) -> Option<Cursor> {
    (!after.is_empty()).then(|| Cursor {
        n1: 0,
        scope: scope.to_string(),
        id: after.to_string(),
    })
}

/// Pages a plain id-ordered list: `rows` were fetched with `limit + 1`.
pub(crate) fn paged(mut rows: Vec<Row>, limit: usize) -> (Vec<Row>, String) {
    if rows.len() > limit {
        rows.truncate(limit);
        let token = rows.last().map(|r| r.id.clone()).unwrap_or_default();
        (rows, token)
    } else {
        (rows, String::new())
    }
}

/// Reads every row a scan selects, in chunks, up to [`SCAN_CAP`].
pub(crate) async fn scan_all(tx: &mut Tx, t: &TableDef, q: Scan) -> Result<Vec<Row>> {
    let mut out: Vec<Row> = Vec::new();
    let mut after = q.after.clone();
    loop {
        let mut chunk = q.clone();
        chunk.after = after.clone();
        chunk.limit = CHUNK;
        let rows = tx.scan(t, &chunk).await?;
        let n = rows.len();
        after = rows.last().map(Cursor::after);
        out.extend(rows);
        if n < CHUNK || out.len() >= SCAN_CAP {
            return Ok(out);
        }
    }
}

/// Like [`scan_all`], decoding every body.
pub(crate) async fn scan_all_msgs<M: prost::Message + Default>(
    tx: &mut Tx,
    t: &TableDef,
    q: Scan,
) -> Result<Vec<(M, Row)>> {
    scan_all(tx, t, q)
        .await?
        .into_iter()
        .map(|r| Ok((r.decode::<M>()?, r)))
        .collect()
}

/// Registers an object id under its kind so it can be found without its scope.
pub(crate) async fn register_id(tx: &mut Tx, kind: &str, id: &str, scope_key: &str) -> Result<()> {
    let row = Row::new(kind, id).k1(scope_key);
    tx.put(&IDS, &row).await?;
    Ok(())
}

/// The scope key an object id was registered with.
pub(crate) async fn lookup_id(tx: &mut Tx, kind: &str, id: &str) -> Result<Option<String>> {
    Ok(tx.get(&IDS, kind, id).await?.and_then(|r| r.k1))
}

/// Forgets an object id.
pub(crate) async fn forget_id(tx: &mut Tx, kind: &str, id: &str) -> Result<()> {
    tx.delete(&IDS, kind, id).await?;
    Ok(())
}

/// Moves a counter past `at_least` (never back) and returns its value.
async fn bump_counter(tx: &mut Tx, scope: &str, name: &str, at_least: u64) -> Result<u64> {
    let cur = tx.add_n1(&COUNTERS, scope, name, 0).await?;
    if at_least as i64 > cur {
        return Ok(tx
            .add_n1(&COUNTERS, scope, name, at_least as i64 - cur)
            .await? as u64);
    }
    Ok(cur as u64)
}

/// The next value of a counter.
pub(crate) async fn next_counter(tx: &mut Tx, scope: &str, name: &str) -> Result<u64> {
    Ok(tx.add_n1(&COUNTERS, scope, name, 1).await? as u64)
}

/// Allocates a number in the repo's shared space inside `tx`.
pub(crate) async fn allocate(
    tx: &mut Tx,
    repo_id: &str,
    kind: NumberKind,
    subject_id: &str,
) -> Result<u64> {
    let n = next_counter(tx, repo_id, "numbers").await?;
    let row = Row::new(repo_id, pad(n))
        .k1(subject_id)
        .msg(&model::NumberRec::new(kind, subject_id));
    tx.put(&NUMBERS, &row).await?;
    Ok(n)
}

/// Claims `number` inside `tx`.
pub(crate) async fn reserve(
    tx: &mut Tx,
    repo_id: &str,
    number: u64,
    kind: NumberKind,
    subject_id: &str,
) -> Result<()> {
    if number == 0 {
        return Err(ForgeError::invalid("invalid_number", "numbers start at 1"));
    }
    if tx.get(&NUMBERS, repo_id, &pad(number)).await?.is_some() {
        return Err(ForgeError::already_exists(format!("number {number}")));
    }
    bump_counter(tx, repo_id, "numbers", number).await?;
    let row = Row::new(repo_id, pad(number))
        .k1(subject_id)
        .msg(&model::NumberRec::new(kind, subject_id));
    tx.put(&NUMBERS, &row).await?;
    Ok(())
}

/// Remembers a repo's current name, so work done without a service call (automation,
/// events) can still show real names.
pub(crate) async fn note_repo(tx: &mut Tx, repo: &RepoInfo) -> Result<()> {
    match tx.get(&IDS, "repo", &repo.id).await? {
        Some(r) if r.k1.as_deref() == Some(repo.full_name.as_str()) => Ok(()),
        _ => register_id(tx, "repo", &repo.id, &repo.full_name).await,
    }
}

/// The repo as last seen by a service call (a placeholder name when never seen).
pub(crate) async fn repo_hint(tx: &mut Tx, repo_id: &str, org_id: &str) -> Result<RepoInfo> {
    Ok(RepoInfo {
        id: repo_id.to_string(),
        org_id: org_id.to_string(),
        full_name: lookup_id(tx, "repo", repo_id)
            .await?
            .unwrap_or_else(|| placeholder(repo_id)),
    })
}

/// A repo reference by id.
pub(crate) fn repo_ref_id(id: &str) -> RepoRef {
    RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    }
}

/// A subject reference in a repo.
pub(crate) fn subject(kind: SubjectKind, repo_id: &str, id: &str) -> SubjectRef {
    SubjectRef {
        kind: kind as i32,
        repo: (!repo_id.is_empty()).then(|| repo_ref_id(repo_id)),
        id: id.to_string(),
    }
}

/// The placeholder name of a repo the call is not about.
pub(crate) fn placeholder(repo_id: &str) -> String {
    format!("{NAME_PLACEHOLDER}{repo_id}")
}

/// An `IssueRef` by id.
pub(crate) fn issue_ref(repo_id: &str, number: u64, id: &str) -> IssueRef {
    IssueRef {
        repo: Some(repo_ref_id(repo_id)),
        number,
        id: id.to_string(),
    }
}

/// What one state change records.
pub(crate) struct Change<'a> {
    /// Event kind, e.g. `issue.closed`.
    pub kind: &'a str,
    /// Audit action, e.g. `issue.close`.
    pub action: &'a str,
    /// Org of the log (and audit chain).
    pub org_id: &'a str,
    /// Repo of the log, empty for org-level objects.
    pub repo_id: &'a str,
    /// Subject kind.
    pub subject_kind: SubjectKind,
    /// Subject id.
    pub subject_id: &'a str,
    /// The resource as proto3-JSON.
    pub payload: String,
    /// Audit summary (JSON).
    pub after: String,
}

/// Records the event and the audit entry of a change in the transaction's outbox.
pub(crate) async fn record(tx: &mut Tx, c: Change<'_>) -> Result<()> {
    tx.emit_event(Event {
        kind: c.kind.into(),
        repo_id: c.repo_id.into(),
        org_id: c.org_id.into(),
        subject: Some(subject(c.subject_kind, c.repo_id, c.subject_id)),
        payload_json: c.payload,
        ..Default::default()
    })
    .await?;
    tx.audit(
        AuditDraft::new(c.action, c.org_id)
            .repo_id(c.repo_id)
            .target(c.subject_kind, c.subject_id)
            .after(&c.after),
    )
    .await
}

/// `RESOURCE_EXHAUSTED` with a `limit_exceeded` payload, for per-object caps.
pub(crate) fn exhausted(limit: &str, max: usize, actual: usize) -> ForgeError {
    ForgeError::new(
        bee_proto::v1::ErrorReason::ResourceExhausted,
        format!("{limit} is {actual}, above the limit of {max}"),
    )
    .with_subcode("limit_exceeded")
    .with_payload(bee_proto::v1::error_detail::Payload::LimitExceeded(
        bee_proto::v1::LimitExceeded {
            limit: limit.to_string(),
            maximum: max as u64,
            actual: actual as u64,
        },
    ))
}

/// Checks an optimistic version (0 = any).
pub(crate) fn check_version(expected: u64, current: u64) -> Result<()> {
    if expected != 0 && expected != current {
        return Err(ForgeError::version_mismatch(expected, current));
    }
    Ok(())
}

/// Limits on lengths, so rows stay bounded.
pub mod limits {
    /// Longest issue or item title.
    pub const TITLE: usize = 1024;
    /// Longest issue, comment or readme body.
    pub const BODY: usize = 262_144;
    /// Most assignees on an issue.
    pub const ASSIGNEES: usize = 10;
    /// Most labels on an issue.
    pub const LABELS: usize = 100;
    /// Most pinned issues per repo.
    pub const PINS: usize = 3;
    /// Most direct sub-issues.
    pub const SUB_ISSUES: usize = 100;
    /// Deepest sub-issue nesting.
    pub const SUB_DEPTH: usize = 8;
    /// Most dependency edges per side.
    pub const DEPENDENCIES: usize = 50;
    /// Most custom fields per org.
    pub const FIELDS: usize = 50;
    /// Most options of one single-select field.
    pub const OPTIONS: usize = 50;
    /// Most issue types per org.
    pub const TYPES: usize = 25;
    /// Most saved replies per user.
    pub const SAVED_REPLIES: usize = 100;
    /// Most items in one project.
    pub const PROJECT_ITEMS: usize = 50_000;
    /// Most fields in one project.
    pub const PROJECT_FIELDS: usize = 50;
    /// Most views in one project.
    pub const VIEWS: usize = 50;
    /// Most workflows in one project.
    pub const WORKFLOWS: usize = 50;
    /// Most charts in one project.
    pub const CHARTS: usize = 50;
}

/// Validates a name: non-empty after trimming, bounded, no control characters.
pub(crate) fn valid_name(what: &str, s: &str, max: usize) -> Result<String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(ForgeError::missing(what));
    }
    if t.chars().count() > max || t.chars().any(char::is_control) {
        return Err(ForgeError::invalid(
            "invalid_name",
            format!("`{what}` must be at most {max} characters without control characters"),
        ));
    }
    Ok(t.to_string())
}

/// Validates a six-hex-digit color (an empty value picks `default`).
pub(crate) fn valid_color(s: &str, default: &str) -> Result<String> {
    let t = s.trim().trim_start_matches('#');
    if t.is_empty() {
        return Ok(default.to_string());
    }
    if t.len() == 6 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(t.to_lowercase());
    }
    Err(ForgeError::invalid(
        "invalid_color",
        "colors are six hex digits without '#'",
    ))
}

/// Maps names to ids for placeholder replacement (used by the service).
pub type Names = BTreeMap<String, String>;
