//! Issues: records, rendering, comments, reactions, timeline, locking, pinning, transfer.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::{
    Comment, CommentEdit, Issue, IssueFieldValue, IssueLockReason, IssueState, IssueStateReason,
    IssueSummary, IssueTaskListProgress, IssueTimelineEvent, IssueTimelineEventKind as Tk, Label,
    Milestone, Principal, Reaction, ReactionContent, ReactionGroup, SubjectKind,
};

use super::assign::AgentAssignment;
use super::model::{IssueRec, NumberKind, NumberRec, ReactionCount, RefRec};
use super::*;
use crate::ids::ts;

/// An issue named by id or by repo and number (already resolved to a repo id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueTarget {
    /// By issue id.
    Id(String),
    /// By repo id and number.
    Number {
        /// Repo id.
        repo_id: String,
        /// Number.
        number: u64,
    },
}

/// A mention or reference found in text, resolved by the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRef {
    /// Repo of the referenced issue.
    pub repo_id: String,
    /// Its number.
    pub number: u64,
}

/// What the text of a body or comment implies: mentioned principals and the issues it names.
/// The service resolves handles and repo names (it can read the directory and the repos).
#[derive(Clone, Debug, Default)]
pub struct TextEffects {
    /// Mentioned principals, resolved.
    pub mentions: Vec<Principal>,
    /// Referenced issues the reader of the target may be told about.
    pub refs: Vec<ResolvedRef>,
}

/// A new issue.
#[derive(Clone, Debug, Default)]
pub struct NewIssue {
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Assignees (resolved).
    pub assignees: Vec<Principal>,
    /// Agent runs to start (the admitted agents among the assignees).
    pub agent_runs: Vec<AgentAssignment>,
    /// Label names.
    pub labels: Vec<String>,
    /// Milestone number, 0 = none.
    pub milestone_number: u64,
    /// Type name or id.
    pub issue_type: String,
    /// Parent for a new sub-issue.
    pub parent: Option<IssueTarget>,
    /// Custom field values.
    pub field_values: Vec<IssueFieldValue>,
    /// Template file the issue was filed from.
    pub template: String,
    /// Text effects of the body.
    pub effects: TextEffects,
    /// Projects to add the issue to.
    pub project_ids: Vec<String>,
    /// The post that inspired the issue, already checked by
    /// [`crate::posts::Posts::inspiration`]; recorded as an adoption with the issue.
    pub inspired_by: Option<bee_proto::v1::PostRef>,
}

/// A comment of an imported issue.
#[derive(Clone, Debug, Default)]
pub struct ImportedComment {
    /// Author.
    pub author: Principal,
    /// Body.
    pub body: String,
    /// Original creation time (ms).
    pub created_ms: u64,
}

/// An issue brought in from another forge with its number and history (SPEC §17.3).
#[derive(Clone, Debug, Default)]
pub struct ImportedIssue {
    /// The number to keep.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
    /// Author.
    pub author: Principal,
    /// Original creation time (ms).
    pub created_ms: u64,
    /// Closed state and when.
    pub closed_ms: Option<u64>,
    /// Close reason (`COMPLETED` when unspecified).
    pub state_reason: IssueStateReason,
    /// Assignees.
    pub assignees: Vec<Principal>,
    /// Label names with colors; missing labels are created.
    pub labels: Vec<(String, String)>,
    /// Milestone title; a missing milestone is created.
    pub milestone: String,
    /// Locked on the source.
    pub locked: bool,
    /// Comments in order.
    pub comments: Vec<ImportedComment>,
}

/// A page of issues.
#[derive(Clone, Debug, Default)]
pub struct IssuePage {
    /// The issues.
    pub issues: Vec<Issue>,
    /// All matches.
    pub total: u64,
    /// Token of the next page, empty at the end.
    pub next: String,
}

/// A page of timeline events.
#[derive(Clone, Debug, Default)]
pub struct TimelinePage {
    /// Events, oldest first.
    pub events: Vec<IssueTimelineEvent>,
    /// Next page token.
    pub next: String,
}

pub(crate) fn is_open(rec: &IssueRec) -> bool {
    rec.state != IssueState::Closed as i32
}

fn pid(p: &Option<Principal>) -> &str {
    p.as_ref().map(|p| p.id.as_str()).unwrap_or("")
}

/// Per-call lookups, loaded on first use.
pub(crate) struct Cache {
    pub viewer: String,
    names: BTreeMap<String, String>,
    labels: BTreeMap<String, BTreeMap<String, Label>>,
    milestones: BTreeMap<String, BTreeMap<u64, Milestone>>,
    types: BTreeMap<String, BTreeMap<String, bee_proto::v1::IssueType>>,
    fields: BTreeMap<String, BTreeMap<String, bee_proto::v1::IssueField>>,
}

impl Cache {
    pub fn new(repo: &RepoInfo, viewer: &str) -> Cache {
        let mut names = BTreeMap::new();
        names.insert(repo.id.clone(), repo.full_name.clone());
        Cache {
            viewer: viewer.to_string(),
            names,
            labels: BTreeMap::new(),
            milestones: BTreeMap::new(),
            types: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }

    /// A cache that knows no repo names: every summary carries placeholders (what the
    /// timeline stores, so renames never leave stale names behind).
    pub fn bare(viewer: &str) -> Cache {
        Cache {
            viewer: viewer.to_string(),
            names: BTreeMap::new(),
            labels: BTreeMap::new(),
            milestones: BTreeMap::new(),
            types: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }

    pub fn with_name(mut self, repo_id: &str, name: &str) -> Cache {
        self.names.insert(repo_id.to_string(), name.to_string());
        self
    }

    pub fn name(&self, repo_id: &str) -> String {
        self.names
            .get(repo_id)
            .cloned()
            .unwrap_or_else(|| placeholder(repo_id))
    }

    pub async fn labels(&mut self, tx: &mut Tx, repo_id: &str) -> Result<&BTreeMap<String, Label>> {
        if !self.labels.contains_key(repo_id) {
            let rows = scan_all_msgs::<Label>(tx, &LABELS, Scan::scope(repo_id, 1)).await?;
            self.labels.insert(
                repo_id.to_string(),
                rows.into_iter().map(|(l, _)| (l.id.clone(), l)).collect(),
            );
        }
        Ok(&self.labels[repo_id])
    }

    pub async fn milestones(
        &mut self,
        tx: &mut Tx,
        repo_id: &str,
    ) -> Result<&BTreeMap<u64, Milestone>> {
        if !self.milestones.contains_key(repo_id) {
            let rows = scan_all_msgs::<Milestone>(tx, &MILESTONES, Scan::scope(repo_id, 1)).await?;
            self.milestones.insert(
                repo_id.to_string(),
                rows.into_iter().map(|(m, _)| (m.number, m)).collect(),
            );
        }
        Ok(&self.milestones[repo_id])
    }

    pub async fn types(
        &mut self,
        tx: &mut Tx,
        org_id: &str,
    ) -> Result<&BTreeMap<String, bee_proto::v1::IssueType>> {
        if !self.types.contains_key(org_id) {
            let rows =
                scan_all_msgs::<bee_proto::v1::IssueType>(tx, &TYPES, Scan::scope(org_id, 1))
                    .await?;
            self.types.insert(
                org_id.to_string(),
                rows.into_iter().map(|(t, _)| (t.id.clone(), t)).collect(),
            );
        }
        Ok(&self.types[org_id])
    }

    pub async fn fields(
        &mut self,
        tx: &mut Tx,
        org_id: &str,
    ) -> Result<&BTreeMap<String, bee_proto::v1::IssueField>> {
        if !self.fields.contains_key(org_id) {
            let rows =
                scan_all_msgs::<bee_proto::v1::IssueField>(tx, &FIELDS, Scan::scope(org_id, 1))
                    .await?;
            self.fields.insert(
                org_id.to_string(),
                rows.into_iter().map(|(f, _)| (f.id.clone(), f)).collect(),
            );
        }
        Ok(&self.fields[org_id])
    }
}

/// Loads an issue by repo and number (no redirects); `NOT_FOUND` otherwise.
pub(crate) async fn load_issue(
    tx: &mut Tx,
    repo_id: &str,
    number: u64,
    lock: bool,
) -> Result<(IssueRec, Row)> {
    let missing = || ForgeError::not_found(format!("issue #{number}"));
    let entry = tx
        .get_msg::<NumberRec>(&NUMBERS, repo_id, &pad(number))
        .await?
        .map(|(e, _)| e)
        .ok_or_else(missing)?;
    if entry.kind() != NumberKind::Issue {
        return Err(missing());
    }
    let row = if lock {
        tx.get_for_update(&ISSUES, repo_id, &entry.subject_id)
            .await?
    } else {
        tx.get(&ISSUES, repo_id, &entry.subject_id).await?
    }
    .ok_or_else(missing)?;
    Ok((row.decode()?, row))
}

/// Loads an issue by id, in whatever repo it lives.
pub(crate) async fn load_issue_id(
    tx: &mut Tx,
    id: &str,
    lock: bool,
) -> Result<Option<(IssueRec, Row)>> {
    let Some(repo_id) = lookup_id(tx, "issue", id).await? else {
        return Ok(None);
    };
    let row = if lock {
        tx.get_for_update(&ISSUES, &repo_id, id).await?
    } else {
        tx.get(&ISSUES, &repo_id, id).await?
    };
    match row {
        Some(row) => Ok(Some((row.decode()?, row))),
        None => Ok(None),
    }
}

/// Resolves an [`IssueTarget`].
pub(crate) async fn load_target(
    tx: &mut Tx,
    t: &IssueTarget,
    lock: bool,
) -> Result<(IssueRec, Row)> {
    match t {
        IssueTarget::Id(id) => load_issue_id(tx, id, lock)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("issue {id}"))),
        IssueTarget::Number { repo_id, number } => load_issue(tx, repo_id, *number, lock).await,
    }
}

/// Writes an issue (insert at `version` 0) and keeps the derived rows right: milestone
/// counters and project item snapshots. Returns the new row version.
pub(crate) async fn save_issue(
    tx: &mut Tx,
    old: Option<&IssueRec>,
    rec: &mut IssueRec,
    version: u64,
) -> Result<u64> {
    rec.updated_ms = tx.now_ms();
    let row = Row::new(rec.repo_id.clone(), rec.id.clone())
        .k1(pad(rec.number))
        .k2(rec.org_id.clone())
        .n1(rec.updated_ms as i64)
        .version(version)
        .msg(rec);
    let v = if version == 0 {
        tx.insert(&ISSUES, &row).await?;
        register_id(tx, "issue", &rec.id, &rec.repo_id).await?;
        1
    } else {
        tx.update(&ISSUES, &row).await?
    };
    super::labels::track_milestone(tx, old, rec).await?;
    super::items::refresh_issue(tx, rec).await?;
    Ok(v)
}

/// Appends a timeline event.
pub(crate) async fn add_timeline(
    tx: &mut Tx,
    issue_id: &str,
    kind: Tk,
    actor: &Principal,
    fill: impl FnOnce(&mut IssueTimelineEvent),
) -> Result<()> {
    let id = tx.new_id()?;
    let mut ev = IssueTimelineEvent {
        id: id.clone(),
        kind: kind as i32,
        actor: Some(actor.clone()),
        created_at: Some(ts(tx.now_ms())),
        ..Default::default()
    };
    fill(&mut ev);
    let row = Row::new(issue_id, id)
        .k1((kind as i32).to_string())
        .msg(&ev);
    tx.insert(&TIMELINE, &row).await
}

fn groups(rec: &[ReactionCount], mine: &BTreeSet<i32>) -> Vec<ReactionGroup> {
    rec.iter()
        .filter(|r| r.count > 0)
        .map(|r| ReactionGroup {
            content: r.content,
            count: r.count,
            viewer_reacted: mine.contains(&r.content),
        })
        .collect()
}

/// Reaction contents `viewer` placed on `subject_id`.
async fn viewer_reactions(tx: &mut Tx, subject_id: &str, viewer: &str) -> Result<BTreeSet<i32>> {
    if viewer.is_empty() {
        return Ok(BTreeSet::new());
    }
    let rows = tx
        .scan_msgs::<Reaction>(
            &REACTIONS,
            &Scan::scope(subject_id, 16).key(Key::K1(viewer.to_string())),
        )
        .await?;
    Ok(rows.into_iter().map(|(r, _)| r.content).collect())
}

/// A short identity of an issue.
pub(crate) async fn summary(tx: &mut Tx, c: &mut Cache, rec: &IssueRec) -> Result<IssueSummary> {
    let type_name = if rec.issue_type_id.is_empty() {
        String::new()
    } else {
        c.types(tx, &rec.org_id)
            .await?
            .get(&rec.issue_type_id)
            .map(|t| t.name.clone())
            .unwrap_or_default()
    };
    let name = c.name(&rec.repo_id);
    Ok(IssueSummary {
        id: rec.id.clone(),
        url: format!("/{name}/issues/{}", rec.number),
        repo_full_name: name,
        number: rec.number,
        title: rec.title.clone(),
        state: rec.state,
        state_reason: rec.state_reason,
        issue_type_name: type_name,
        assignees: rec.assignees.clone(),
    })
}

/// The wire form of a stored issue.
pub(crate) async fn render(
    tx: &mut Tx,
    c: &mut Cache,
    rec: &IssueRec,
    version: u64,
) -> Result<Issue> {
    let name = c.name(&rec.repo_id);
    let labels_all = c.labels(tx, &rec.repo_id).await?;
    let labels: Vec<Label> = rec
        .label_ids
        .iter()
        .filter_map(|id| labels_all.get(id).cloned())
        .collect();
    let milestone = if rec.milestone_number > 0 {
        c.milestones(tx, &rec.repo_id)
            .await?
            .get(&rec.milestone_number)
            .cloned()
    } else {
        None
    };
    let issue_type = if rec.issue_type_id.is_empty() {
        None
    } else {
        c.types(tx, &rec.org_id)
            .await?
            .get(&rec.issue_type_id)
            .cloned()
    };
    let parent = if rec.parent_id.is_empty() {
        None
    } else {
        match load_issue_id(tx, &rec.parent_id, false).await? {
            Some((p, _)) => Some(summary(tx, c, &p).await?),
            None => None,
        }
    };
    let sub_issues = if rec.child_count > 0 {
        Some(super::relations::sub_progress(tx, &rec.id).await?)
    } else {
        None
    };
    let dependencies = if rec.blocked_by_count + rec.blocking_count > 0 {
        Some(super::relations::dep_summary(tx, rec).await?)
    } else {
        None
    };
    let (total, completed) = text::task_list_progress(&rec.body);
    let mut field_values = Vec::new();
    if !rec.field_values.is_empty() {
        let fields = c.fields(tx, &rec.org_id).await?;
        for v in &rec.field_values {
            if let Some(f) = fields.get(&v.field_id) {
                let mut v = v.clone();
                v.field_name = f.name.clone();
                v.kind = f.kind;
                field_values.push(v);
            }
        }
    }
    let mine = if rec.reaction_total > 0 {
        viewer_reactions(tx, &rec.id, &c.viewer.clone()).await?
    } else {
        BTreeSet::new()
    };
    let ts_opt = |ms: u64| (ms > 0).then(|| ts(ms));
    Ok(Issue {
        id: rec.id.clone(),
        repo_id: rec.repo_id.clone(),
        url: format!("/{name}/issues/{}", rec.number),
        repo_full_name: name,
        number: rec.number,
        title: rec.title.clone(),
        body: rec.body.clone(),
        body_html: String::new(),
        state: rec.state,
        state_reason: rec.state_reason,
        duplicate_of: rec
            .duplicate_of
            .as_ref()
            .map(|d| issue_ref(&d.repo_id, d.number, &d.issue_id)),
        author: rec.author.clone(),
        author_association: rec.author_association.clone(),
        assignees: rec.assignees.clone(),
        labels,
        milestone,
        issue_type,
        parent,
        sub_issues,
        dependencies,
        task_list: Some(IssueTaskListProgress { total, completed }),
        field_values,
        locked: rec.locked,
        lock_reason: rec.lock_reason,
        pinned: rec.pinned,
        comment_count: rec.comment_count,
        reactions: groups(&rec.reactions, &mine),
        linked_change_request_ids: rec.linked_change_ids.clone(),
        active_agent_run_ids: rec
            .active_agent_runs
            .iter()
            .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
            .collect(),
        created_at: ts_opt(rec.created_ms),
        updated_at: ts_opt(rec.updated_ms),
        closed_at: ts_opt(rec.closed_ms),
        closed_by: rec.closed_by.clone(),
        transferred_from: rec
            .transferred_from
            .as_ref()
            .map(|t| issue_ref(&t.repo_id, t.number, &t.issue_id)),
        version,
        inspired_by: None,
    })
}

fn validate_title(t: &str) -> Result<String> {
    let t = t.trim();
    if t.is_empty() {
        return Err(ForgeError::missing("title"));
    }
    if t.chars().count() > limits::TITLE {
        return Err(ForgeError::invalid(
            "title_too_long",
            format!("titles are at most {} characters", limits::TITLE),
        ));
    }
    Ok(t.to_string())
}

fn validate_body(b: &str) -> Result<()> {
    if b.len() > limits::BODY {
        return Err(ForgeError::too_large(
            "body_bytes",
            limits::BODY as u64,
            b.len() as u64,
        ));
    }
    Ok(())
}

fn issue_audit(rec: &IssueRec) -> String {
    serde_json::json!({"number": rec.number, "title": rec.title, "state": rec.state}).to_string()
}

fn org_scope_change<'a>(
    repo: &'a RepoInfo,
    kind: &'a str,
    action: &'a str,
    rec: &'a IssueRec,
    payload: String,
) -> Change<'a> {
    Change {
        kind,
        action,
        org_id: &repo.org_id,
        repo_id: &repo.id,
        subject_kind: SubjectKind::Issue,
        subject_id: &rec.id,
        payload,
        after: issue_audit(rec),
    }
}

/// Applies mentions and references found in `text` of `rec` (or one of its comments).
pub(crate) async fn apply_effects(
    tx: &mut Tx,
    repo: &RepoInfo,
    rec: &mut IssueRec,
    source: SubjectRef,
    actor: &Principal,
    fx: &TextEffects,
) -> Result<()> {
    for m in &fx.mentions {
        if m.id == actor.id || rec.mentioned_ids.contains(&m.id) {
            continue;
        }
        rec.mentioned_ids.push(m.id.clone());
        let (src, who) = (source.clone(), m.clone());
        add_timeline(tx, &rec.id, Tk::Mentioned, actor, |e| {
            e.source = Some(src);
            e.current_value = who.display.clone();
            e.mentioned_user = Some(who.clone());
            e.assignee = Some(who);
        })
        .await?;
    }
    for r in &fx.refs {
        if r.repo_id == repo.id && r.number == rec.number {
            continue;
        }
        let Ok((target, _)) = load_issue(tx, &r.repo_id, r.number, false).await else {
            continue;
        };
        let existing = tx
            .scan_msgs::<IssueTimelineEvent>(
                &TIMELINE,
                &Scan::scope(target.id.clone(), 1000)
                    .key(Key::K1((Tk::CrossReferenced as i32).to_string())),
            )
            .await?;
        let dup = existing.iter().any(|(e, _)| {
            e.source
                .as_ref()
                .is_some_and(|s| s.id == source.id && s.kind == source.kind)
        });
        if dup {
            continue;
        }
        let src = source.clone();
        add_timeline(tx, &target.id, Tk::CrossReferenced, actor, |e| {
            e.source = Some(src)
        })
        .await?;
        let _ = repo;
    }
    Ok(())
}

fn dedupe_principals(v: &[Principal]) -> Vec<Principal> {
    let mut seen = BTreeSet::new();
    v.iter()
        .filter(|p| seen.insert(p.id.clone()))
        .cloned()
        .collect()
}

/// Opens an issue inside a transaction (the body of [`Work::create_issue`]).
pub(crate) async fn create_issue_in_tx(
    tx: &mut Tx,
    repo: &RepoInfo,
    actor: &Actor,
    new: NewIssue,
) -> Result<Issue> {
    let title = validate_title(&new.title)?;
    validate_body(&new.body)?;
    let assignees = dedupe_principals(&new.assignees);
    if assignees.len() > limits::ASSIGNEES {
        return Err(exhausted("assignees", limits::ASSIGNEES, assignees.len()));
    }
    note_repo(tx, repo).await?;
    let id = tx.new_id()?;
    let now = tx.now_ms();
    let mut rec = IssueRec {
        id: id.clone(),
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        title,
        body: new.body.clone(),
        state: IssueState::Open as i32,
        author: Some(actor.principal.clone()),
        author_association: actor.association.clone(),
        assignees: assignees.clone(),
        created_ms: now,
        ..Default::default()
    };
    // Labels, milestone, type and fields resolve against this repo and org.
    rec.label_ids = super::labels::resolve_labels(tx, &repo.id, &new.labels, false).await?;
    if rec.label_ids.len() > limits::LABELS {
        return Err(exhausted("labels", limits::LABELS, rec.label_ids.len()));
    }
    if new.milestone_number > 0 {
        super::labels::require_milestone(tx, &repo.id, new.milestone_number).await?;
        rec.milestone_number = new.milestone_number;
    }
    if !new.issue_type.trim().is_empty() {
        rec.issue_type_id =
            super::fields::resolve_type(tx, &repo.org_id, &new.issue_type, true).await?;
    }
    if !new.field_values.is_empty() {
        rec.field_values = super::fields::validate_values(
            tx,
            &repo.org_id,
            &rec.issue_type_id,
            &[],
            &new.field_values,
        )
        .await?;
    }
    rec.number = allocate(tx, &repo.id, NumberKind::Issue, &id).await?;
    let me = actor.principal.clone();
    for l in rec.label_ids.clone() {
        let label = lookup_label(tx, &repo.id, &l).await?;
        add_timeline(tx, &id, Tk::Labeled, &me, |e| e.label = label).await?;
    }
    for a in &assignees {
        let a = a.clone();
        add_timeline(tx, &id, Tk::Assigned, &me, |e| e.assignee = Some(a)).await?;
    }
    if rec.milestone_number > 0 {
        let title = super::labels::milestone_title(tx, &repo.id, rec.milestone_number).await?;
        add_timeline(tx, &id, Tk::Milestoned, &me, |e| e.milestone_title = title).await?;
    }
    if !rec.issue_type_id.is_empty() {
        let name = type_name(tx, &repo.org_id, &rec.issue_type_id).await?;
        add_timeline(tx, &id, Tk::TypeChanged, &me, |e| e.current_value = name).await?;
    }
    let source = subject(SubjectKind::Issue, &repo.id, &id);
    apply_effects(tx, repo, &mut rec, source, &me, &new.effects).await?;
    super::assign::start_runs(tx, repo, &mut rec, &me, &new.agent_runs).await?;
    save_issue(tx, None, &mut rec, 0).await?;
    if let Some(parent) = &new.parent {
        super::relations::attach_in_tx(
            tx,
            repo,
            &me,
            parent,
            &IssueTarget::Id(id.clone()),
            false,
            "",
        )
        .await?;
    }
    for p in &new.project_ids {
        let (cur, _) = load_issue_id(tx, &id, false)
            .await?
            .ok_or_else(|| ForgeError::internal("issue vanished"))?;
        super::items::add_issue_in_tx(tx, repo, p, &cur, &me).await?;
    }
    {
        let (cur, _) = load_issue_id(tx, &id, false)
            .await?
            .ok_or_else(|| ForgeError::internal("issue vanished"))?;
        super::automation::on_issue_created(tx, repo, &cur).await?;
    }
    if let Some(post) = &new.inspired_by {
        crate::posts::adopt::record(
            tx,
            &crate::posts::adopt::AdoptionRec {
                post: Some(post.clone()),
                repo_id: repo.id.clone(),
                org_id: repo.org_id.clone(),
                kind: SubjectKind::Issue as i32,
                subject_id: id.clone(),
                number: rec.number,
                title: rec.title.clone(),
                author: Some(actor.principal.clone()),
                created_ms: now,
            },
        )
        .await?;
    }
    let (fin, frow) = load_issue_id(tx, &id, false)
        .await?
        .ok_or_else(|| ForgeError::internal("issue vanished"))?;
    let mut cache = Cache::new(repo, &actor.principal.id);
    let issue = render(tx, &mut cache, &fin, frow.version).await?;
    let mut change = org_scope_change(repo, "issue.opened", "issue.create", &fin, pjson!(&issue));
    if !new.template.is_empty() {
        change.after =
            serde_json::json!({"number": fin.number, "title": fin.title, "template": new.template})
                .to_string();
    }
    record(tx, change).await?;
    Ok(issue)
}

/// Closes an issue inside a transaction (automation and landed change requests). A closed
/// issue is left alone.
pub(crate) async fn close_issue_in_tx(
    tx: &mut Tx,
    issue_id: &str,
    actor: &Principal,
    reason: IssueStateReason,
    by_change: &str,
) -> Result<()> {
    let Some((mut rec, row)) = load_issue_id(tx, issue_id, true).await? else {
        return Ok(());
    };
    if !is_open(&rec) {
        return Ok(());
    }
    let old = rec.clone();
    rec.state = IssueState::Closed as i32;
    rec.state_reason = reason as i32;
    rec.closed_ms = tx.now_ms();
    rec.closed_by = Some(actor.clone());
    let cid = by_change.to_string();
    add_timeline(tx, &rec.id, Tk::Closed, actor, |e| {
        e.state_reason = reason as i32;
        e.change_request_id = cid;
    })
    .await?;
    let v = save_issue(tx, Some(&old), &mut rec, row.version).await?;
    super::automation::on_issue_state(tx, &rec, true, None).await?;
    let repo = repo_hint(tx, &rec.repo_id, &rec.org_id).await?;
    let mut cache = Cache::new(&repo, &actor.id);
    let issue = render(tx, &mut cache, &rec, v).await?;
    record(
        tx,
        org_scope_change(&repo, "issue.closed", "issue.close", &rec, pjson!(&issue)),
    )
    .await
}

impl Work {
    /// Opens an issue.
    pub async fn create_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        new: NewIssue,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let issue = create_issue_in_tx(&mut tx, repo, actor, new).await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Brings in an issue with its number, timestamps and comments (SPEC §17.3). Emits no
    /// events (a migration must not flood webhooks); the audit entry records the import.
    pub async fn import_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        imp: ImportedIssue,
    ) -> Result<Issue> {
        let title = validate_title(&imp.title)?;
        validate_body(&imp.body)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = tx.new_id()?;
        let mut rec = IssueRec {
            id: id.clone(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            number: imp.number,
            title,
            body: imp.body.clone(),
            state: IssueState::Open as i32,
            author: Some(imp.author.clone()),
            author_association: "NONE".into(),
            assignees: dedupe_principals(&imp.assignees),
            created_ms: imp.created_ms,
            locked: imp.locked,
            ..Default::default()
        };
        reserve(&mut tx, &repo.id, imp.number, NumberKind::Issue, &id).await?;
        for (name, color) in &imp.labels {
            let ids =
                super::labels::resolve_labels_with_color(&mut tx, &repo.id, name, color).await?;
            rec.label_ids.push(ids);
        }
        if !imp.milestone.trim().is_empty() {
            rec.milestone_number =
                super::labels::ensure_milestone(&mut tx, &repo.id, &imp.milestone).await?;
        }
        if let Some(closed) = imp.closed_ms {
            rec.state = IssueState::Closed as i32;
            rec.closed_ms = closed;
            rec.state_reason = match imp.state_reason {
                IssueStateReason::Unspecified | IssueStateReason::Reopened => {
                    IssueStateReason::Completed as i32
                }
                r => r as i32,
            };
        }
        save_issue(&mut tx, None, &mut rec, 0).await?;
        // `save_issue` stamps the update time; keep the original for display.
        let (mut stored, row) = load_issue_id(&mut tx, &id, true)
            .await?
            .ok_or_else(|| ForgeError::internal("issue vanished"))?;
        for cm in &imp.comments {
            let cid = tx.new_id()?;
            let comment = Comment {
                id: cid.clone(),
                subject: Some(subject(SubjectKind::Issue, &repo.id, &id)),
                author: Some(cm.author.clone()),
                body: cm.body.clone(),
                created_at: Some(ts(cm.created_ms)),
                updated_at: Some(ts(cm.created_ms)),
                url: format!(
                    "/{}/issues/{}#issuecomment-{cid}",
                    repo.full_name, imp.number
                ),
                author_association: "NONE".into(),
                ..Default::default()
            };
            let row = Row::new(id.clone(), cid.clone())
                .k1(cm.author.id.clone())
                .n1(cm.created_ms as i64)
                .msg(&comment);
            tx.insert(&COMMENTS, &row).await?;
            register_id(&mut tx, "comment", &cid, &id).await?;
            stored.comment_count += 1;
            if !stored.commenter_ids.contains(&cm.author.id) {
                stored.commenter_ids.push(cm.author.id.clone());
            }
            add_timeline(&mut tx, &id, Tk::Commented, &cm.author, |e| {
                e.comment = Some(Comment {
                    id: cid,
                    ..Default::default()
                });
            })
            .await?;
        }
        let old = stored.clone();
        save_issue(&mut tx, Some(&old), &mut stored, row.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let (fin, frow) = load_issue_id(&mut tx, &id, false)
            .await?
            .ok_or_else(|| ForgeError::internal("issue vanished"))?;
        let issue = render(&mut tx, &mut cache, &fin, frow.version).await?;
        tx.audit(
            crate::audit::AuditDraft::new("issue.import", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Issue, &id)
                .after(&issue_audit(&fin)),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Fetches an issue by id.
    pub async fn get_issue(&self, cx: &Cx, repo: &RepoInfo, id: &str) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = load_issue_id(&mut tx, id, false)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("issue {id}")))?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        render(&mut tx, &mut cache, &rec, row.version).await
    }

    /// The repo an issue id lives in.
    pub async fn issue_repo(&self, cx: &Cx, id: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        lookup_id(&mut tx, "issue", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("issue {id}")))
    }

    /// Fetches an issue by number; a transferred issue answers from its new location (the
    /// returned `repo_id` may differ from `repo.id`, and the caller must be allowed to read
    /// it there).
    pub async fn get_issue_by_number(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let entry = tx
            .get_msg::<NumberRec>(&NUMBERS, &repo.id, &pad(number))
            .await?
            .map(|(e, _)| e)
            .ok_or_else(|| ForgeError::not_found(format!("issue #{number}")))?;
        let (rec, row) = match entry.kind() {
            NumberKind::Issue => load_issue(&mut tx, &repo.id, number, false).await?,
            NumberKind::Redirect => {
                load_issue(&mut tx, &entry.to_repo_id, entry.to_number, false).await?
            }
            _ => return Err(ForgeError::not_found(format!("issue #{number}"))),
        };
        let mut cache = Cache::new(repo, &cx.actor_id);
        render(&mut tx, &mut cache, &rec, row.version).await
    }

    /// Edits title and body. The author and triagers may edit.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        title: Option<String>,
        body: Option<String>,
        expected_version: u64,
        effects: &TextEffects,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        check_version(expected_version, row.version)?;
        if !actor.triage && pid(&rec.author) != actor.principal.id {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may edit an issue",
            ));
        }
        let old = rec.clone();
        let mut changed = false;
        let mut renamed = None;
        if let Some(t) = title {
            let t = validate_title(&t)?;
            if t != rec.title {
                renamed = Some((rec.title.clone(), t.clone()));
                rec.title = t;
                changed = true;
            }
        }
        if let Some(b) = body {
            validate_body(&b)?;
            if b != rec.body {
                rec.body = b;
                changed = true;
            }
        }
        let me = actor.principal.clone();
        if changed {
            if let Some((prev, cur)) = renamed {
                add_timeline(&mut tx, &rec.id, Tk::Renamed, &me, |e| {
                    e.previous_title = prev;
                    e.current_title = cur;
                })
                .await?;
            }
            let source = subject(SubjectKind::Issue, &repo.id, &rec.id);
            apply_effects(&mut tx, repo, &mut rec, source, &me, effects).await?;
            let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
            let mut cache = Cache::new(repo, &cx.actor_id);
            let issue = render(&mut tx, &mut cache, &rec, v).await?;
            record(
                &mut tx,
                org_scope_change(repo, "issue.edited", "issue.update", &rec, pjson!(&issue)),
            )
            .await?;
            tx.commit().await?;
            return Ok(issue);
        }
        let mut cache = Cache::new(repo, &cx.actor_id);
        render(&mut tx, &mut cache, &rec, row.version).await
    }

    /// Permanently deletes an issue (repo admins). Its number is never reused.
    pub async fn delete_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
    ) -> Result<()> {
        if !actor.admin {
            return Err(ForgeError::permission_denied(
                "only repo admins may delete issues",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, true).await?;
        super::relations::detach_all(&mut tx, &rec).await?;
        for (_, r) in
            scan_all_msgs::<Comment>(&mut tx, &COMMENTS, Scan::scope(rec.id.clone(), 1)).await?
        {
            purge_comment(&mut tx, &rec.id, &r.id).await?;
        }
        for r in scan_all(&mut tx, &TIMELINE, Scan::scope(rec.id.clone(), 1)).await? {
            tx.delete(&TIMELINE, &rec.id, &r.id).await?;
        }
        for r in scan_all(&mut tx, &REACTIONS, Scan::scope(rec.id.clone(), 1)).await? {
            tx.delete(&REACTIONS, &rec.id, &r.id).await?;
        }
        for r in scan_all(&mut tx, &LINKS, Scan::scope(rec.id.clone(), 1)).await? {
            tx.delete(&LINKS, &rec.id, &r.id).await?;
        }
        for r in scan_all(&mut tx, &BRANCHES, Scan::scope(rec.id.clone(), 1)).await? {
            tx.delete(&BRANCHES, &rec.id, &r.id).await?;
        }
        tx.delete(&PINS, &repo.id, &rec.id).await?;
        super::items::remove_content(&mut tx, &format!("issue:{}", rec.id)).await?;
        let mut gone = rec.clone();
        gone.state = IssueState::Unspecified as i32;
        gone.milestone_number = 0;
        super::labels::track_milestone(&mut tx, Some(&rec), &gone).await?;
        tx.delete(&ISSUES, &repo.id, &rec.id).await?;
        forget_id(&mut tx, "issue", &rec.id).await?;
        crate::posts::adopt::forget_issue(&mut tx, &rec.id).await?;
        let retired = Row::new(repo.id.clone(), pad(number))
            .k1(rec.id.clone())
            .msg(&NumberRec::new(NumberKind::Retired, &rec.id));
        tx.put(&NUMBERS, &retired).await?;
        record(
            &mut tx,
            org_scope_change(
                repo,
                "issue.deleted",
                "issue.delete",
                &rec,
                serde_json::json!({"id": rec.id, "number": rec.number, "repo_id": repo.id})
                    .to_string(),
            ),
        )
        .await?;
        tx.commit().await
    }

    #[allow(clippy::too_many_arguments)]
    async fn state_change(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        expected_version: u64,
        close: Option<(IssueStateReason, Option<IssueTarget>)>,
        comment: &str,
        effects: &TextEffects,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        note_repo(&mut tx, repo).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        check_version(expected_version, row.version)?;
        if !actor.triage && pid(&rec.author) != actor.principal.id {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may change an issue's state",
            ));
        }
        let closing = close.is_some();
        if closing == !is_open(&rec) {
            return Err(ForgeError::invalid_state(if closing {
                "the issue is already closed"
            } else {
                "the issue is already open"
            }));
        }
        let me = actor.principal.clone();
        if !comment.trim().is_empty() {
            if rec.locked && !actor.triage {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::Locked,
                    "the conversation is locked",
                ));
            }
            add_comment_in_tx(&mut tx, repo, actor, &mut rec, comment, effects).await?;
        }
        let old = rec.clone();
        let (kind, action);
        match close {
            Some((reason, dup)) => {
                rec.state = IssueState::Closed as i32;
                rec.state_reason = reason as i32;
                rec.closed_ms = tx.now_ms();
                rec.closed_by = Some(me.clone());
                let mut related = None;
                if reason == IssueStateReason::Duplicate {
                    let target = dup.ok_or_else(|| {
                        ForgeError::invalid(
                            "duplicate_of_required",
                            "`duplicate_of` is required for a duplicate close",
                        )
                    })?;
                    let (t, _) = load_target(&mut tx, &target, false).await?;
                    if t.id == rec.id {
                        return Err(ForgeError::invalid(
                            "self_duplicate",
                            "an issue cannot duplicate itself",
                        ));
                    }
                    rec.duplicate_of = Some(RefRec {
                        repo_id: t.repo_id.clone(),
                        issue_id: t.id.clone(),
                        number: t.number,
                    });
                    related = Some(t);
                }
                if let Some(t) = related {
                    let mut cache = Cache::bare(&cx.actor_id);
                    let s = summary(&mut tx, &mut cache, &t).await?;
                    add_timeline(&mut tx, &rec.id, Tk::MarkedDuplicate, &me, |e| {
                        e.related_issue = Some(s)
                    })
                    .await?;
                }
                add_timeline(&mut tx, &rec.id, Tk::Closed, &me, |e| {
                    e.state_reason = reason as i32
                })
                .await?;
                kind = "issue.closed";
                action = "issue.close";
            }
            None => {
                rec.state = IssueState::Open as i32;
                rec.state_reason = IssueStateReason::Reopened as i32;
                rec.closed_ms = 0;
                rec.closed_by = None;
                rec.duplicate_of = None;
                add_timeline(&mut tx, &rec.id, Tk::Reopened, &me, |e| {
                    e.state_reason = IssueStateReason::Reopened as i32
                })
                .await?;
                kind = "issue.reopened";
                action = "issue.reopen";
            }
        }
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        super::automation::on_issue_state(&mut tx, &rec, closing, None).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            org_scope_change(repo, kind, action, &rec, pjson!(&issue)),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Closes an issue. `duplicate_of` is required for [`IssueStateReason::Duplicate`].
    #[allow(clippy::too_many_arguments)]
    pub async fn close_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        reason: IssueStateReason,
        duplicate_of: Option<IssueTarget>,
        comment: &str,
        expected_version: u64,
        effects: &TextEffects,
    ) -> Result<Issue> {
        let reason = match reason {
            IssueStateReason::Unspecified => IssueStateReason::Completed,
            IssueStateReason::Reopened => {
                return Err(ForgeError::invalid(
                    "invalid_reason",
                    "REOPENED is not a close reason",
                ));
            }
            r => r,
        };
        if duplicate_of.is_some() && reason != IssueStateReason::Duplicate {
            return Err(ForgeError::invalid(
                "invalid_reason",
                "`duplicate_of` is only valid with the DUPLICATE reason",
            ));
        }
        self.state_change(
            cx,
            repo,
            actor,
            number,
            expected_version,
            Some((reason, duplicate_of)),
            comment,
            effects,
        )
        .await
    }

    /// Reopens a closed issue.
    #[allow(clippy::too_many_arguments)]
    pub async fn reopen_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        comment: &str,
        expected_version: u64,
        effects: &TextEffects,
    ) -> Result<Issue> {
        self.state_change(
            cx,
            repo,
            actor,
            number,
            expected_version,
            None,
            comment,
            effects,
        )
        .await
    }

    /// Marks an issue as a duplicate and closes it (an already closed issue only gains the
    /// mark).
    pub async fn mark_duplicate(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        duplicate_of: IssueTarget,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, false).await?;
        let open = is_open(&rec);
        drop(tx);
        if open {
            return self
                .close_issue(
                    cx,
                    repo,
                    actor,
                    number,
                    IssueStateReason::Duplicate,
                    Some(duplicate_of),
                    "",
                    0,
                    &TextEffects::default(),
                )
                .await;
        }
        if !actor.triage && pid(&rec.author) != actor.principal.id {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may mark duplicates",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let (t, _) = load_target(&mut tx, &duplicate_of, false).await?;
        if t.id == rec.id {
            return Err(ForgeError::invalid(
                "self_duplicate",
                "an issue cannot duplicate itself",
            ));
        }
        let old = rec.clone();
        rec.state_reason = IssueStateReason::Duplicate as i32;
        rec.duplicate_of = Some(RefRec {
            repo_id: t.repo_id.clone(),
            issue_id: t.id.clone(),
            number: t.number,
        });
        let mut cache = Cache::new(repo, &cx.actor_id);
        let s = summary(&mut tx, &mut cache, &t).await?;
        add_timeline(
            &mut tx,
            &rec.id,
            Tk::MarkedDuplicate,
            &actor.principal,
            |e| e.related_issue = Some(s),
        )
        .await?;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            org_scope_change(
                repo,
                "issue.duplicate_marked",
                "issue.mark_duplicate",
                &rec,
                pjson!(&issue),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Clears a duplicate mark; the issue stays closed (as completed) unless `reopen`.
    pub async fn unmark_duplicate(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        reopen: bool,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        if rec.duplicate_of.is_none() {
            return Err(ForgeError::invalid_state(
                "the issue is not marked as a duplicate",
            ));
        }
        if !actor.triage && pid(&rec.author) != actor.principal.id {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may clear duplicates",
            ));
        }
        let old = rec.clone();
        let me = actor.principal.clone();
        let prev = rec.duplicate_of.take();
        let mut cache = Cache::new(repo, &cx.actor_id);
        let related = match prev {
            Some(p) => match load_issue_id(&mut tx, &p.issue_id, false).await? {
                Some((t, _)) => Some(summary(&mut tx, &mut cache, &t).await?),
                None => None,
            },
            None => None,
        };
        add_timeline(&mut tx, &rec.id, Tk::UnmarkedDuplicate, &me, |e| {
            e.related_issue = related
        })
        .await?;
        if reopen {
            rec.state = IssueState::Open as i32;
            rec.state_reason = IssueStateReason::Reopened as i32;
            rec.closed_ms = 0;
            rec.closed_by = None;
            add_timeline(&mut tx, &rec.id, Tk::Reopened, &me, |e| {
                e.state_reason = IssueStateReason::Reopened as i32
            })
            .await?;
        } else {
            rec.state_reason = IssueStateReason::Completed as i32;
        }
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        if reopen {
            super::automation::on_issue_state(&mut tx, &rec, false, None).await?;
        }
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            org_scope_change(
                repo,
                "issue.duplicate_unmarked",
                "issue.unmark_duplicate",
                &rec,
                pjson!(&issue),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Locks the conversation.
    pub async fn lock_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        reason: IssueLockReason,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        if rec.locked {
            return Err(ForgeError::invalid_state(
                "the conversation is already locked",
            ));
        }
        let old = rec.clone();
        rec.locked = true;
        rec.lock_reason = reason as i32;
        add_timeline(&mut tx, &rec.id, Tk::Locked, &actor.principal, |e| {
            e.lock_reason = reason as i32
        })
        .await?;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            org_scope_change(repo, "issue.locked", "issue.lock", &rec, pjson!(&issue)),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Unlocks the conversation.
    pub async fn unlock_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        if !rec.locked {
            return Err(ForgeError::invalid_state("the conversation is not locked"));
        }
        let old = rec.clone();
        rec.locked = false;
        rec.lock_reason = 0;
        add_timeline(&mut tx, &rec.id, Tk::Unlocked, &actor.principal, |_| {}).await?;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            org_scope_change(repo, "issue.unlocked", "issue.unlock", &rec, pjson!(&issue)),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Pins an issue (at most three per repo).
    pub async fn pin_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        if rec.pinned {
            let mut cache = Cache::new(repo, &cx.actor_id);
            return render(&mut tx, &mut cache, &rec, row.version).await;
        }
        let pinned = tx.count(&PINS, &repo.id, None).await? as usize;
        if pinned >= limits::PINS {
            return Err(exhausted("pinned_issues", limits::PINS, pinned + 1));
        }
        let order = next_counter(&mut tx, &repo.id, "pins").await?;
        tx.put(
            &PINS,
            &Row::new(repo.id.clone(), rec.id.clone()).n1(order as i64),
        )
        .await?;
        let old = rec.clone();
        rec.pinned = true;
        add_timeline(&mut tx, &rec.id, Tk::Pinned, &actor.principal, |_| {}).await?;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            org_scope_change(repo, "issue.pinned", "issue.pin", &rec, pjson!(&issue)),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Unpins an issue.
    pub async fn unpin_issue(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        if !rec.pinned {
            return render(&mut tx, &mut cache, &rec, row.version).await;
        }
        tx.delete(&PINS, &repo.id, &rec.id).await?;
        let old = rec.clone();
        rec.pinned = false;
        add_timeline(&mut tx, &rec.id, Tk::Unpinned, &actor.principal, |_| {}).await?;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            org_scope_change(repo, "issue.unpinned", "issue.unpin", &rec, pjson!(&issue)),
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Pinned issues in pin order.
    pub async fn list_pinned(&self, cx: &Cx, repo: &RepoInfo) -> Result<Vec<Issue>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let pins = tx
            .scan(
                &PINS,
                &Scan::scope(repo.id.clone(), limits::PINS + 1).by_n1(),
            )
            .await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let mut out = Vec::new();
        for p in pins {
            if let Some(row) = tx.get(&ISSUES, &repo.id, &p.id).await? {
                let rec: IssueRec = row.decode()?;
                out.push(render(&mut tx, &mut cache, &rec, row.version).await?);
            }
        }
        Ok(out)
    }

    /// Moves an issue to another repo of the same owner. The old number redirects.
    pub async fn transfer_issue(
        &self,
        cx: &Cx,
        src: &RepoInfo,
        dst: &RepoInfo,
        actor: &Actor,
        number: u64,
        create_labels: bool,
    ) -> Result<(Issue, u64)> {
        if src.id == dst.id {
            return Err(ForgeError::invalid(
                "same_repo",
                "the target is the issue's own repo",
            ));
        }
        if src.org_id != dst.org_id {
            return Err(ForgeError::invalid(
                "different_owner",
                "issues move only between repos of the same owner",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, _) = load_issue(&mut tx, &src.id, number, true).await?;
        let me = actor.principal.clone();
        let from = RefRec {
            repo_id: src.id.clone(),
            issue_id: rec.id.clone(),
            number,
        };
        // Labels and milestone map by name; the rest is org level and stays.
        let mut new_labels = Vec::new();
        let src_labels =
            scan_all_msgs::<Label>(&mut tx, &LABELS, Scan::scope(src.id.clone(), 1)).await?;
        for lid in &rec.label_ids {
            let Some((l, _)) = src_labels.iter().find(|(l, _)| l.id == *lid) else {
                continue;
            };
            match super::labels::find_label(&mut tx, &dst.id, &l.name).await? {
                Some(t) => new_labels.push(t.id),
                None if create_labels => {
                    let created = super::labels::create_label_in_tx(
                        &mut tx,
                        dst,
                        &me,
                        &l.name,
                        &l.color,
                        &l.description,
                    )
                    .await?;
                    new_labels.push(created.id);
                }
                None => {}
            }
        }
        let mut new_milestone = 0;
        if rec.milestone_number > 0 {
            let title =
                super::labels::milestone_title(&mut tx, &src.id, rec.milestone_number).await?;
            if let Some(m) =
                super::labels::find_milestone_by_title(&mut tx, &dst.id, &title).await?
            {
                new_milestone = m.number;
            }
        }
        let old = rec.clone();
        // Leave the source: counters, pin, row.
        let mut leaving = rec.clone();
        leaving.state = IssueState::Unspecified as i32;
        leaving.milestone_number = 0;
        super::labels::track_milestone(&mut tx, Some(&old), &leaving).await?;
        tx.delete(&PINS, &src.id, &rec.id).await?;
        tx.delete(&ISSUES, &src.id, &rec.id).await?;
        // Arrive: new number, redirect from the old one.
        let new_number = allocate(&mut tx, &dst.id, NumberKind::Issue, &rec.id).await?;
        let redirect = NumberRec {
            kind: NumberKind::Redirect as i32,
            subject_id: rec.id.clone(),
            to_repo_id: dst.id.clone(),
            to_number: new_number,
        };
        tx.put(
            &NUMBERS,
            &Row::new(src.id.clone(), pad(number))
                .k1(rec.id.clone())
                .msg(&redirect),
        )
        .await?;
        rec.repo_id = dst.id.clone();
        rec.number = new_number;
        rec.label_ids = new_labels;
        rec.milestone_number = new_milestone;
        rec.pinned = false;
        rec.transferred_from = Some(from);
        rec.updated_ms = tx.now_ms();
        add_timeline(&mut tx, &rec.id, Tk::Transferred, &me, |e| {
            e.previous_repo_full_name = src.full_name.clone();
        })
        .await?;
        let row = Row::new(dst.id.clone(), rec.id.clone())
            .k1(pad(new_number))
            .k2(rec.org_id.clone())
            .n1(rec.updated_ms as i64)
            .msg(&rec);
        tx.insert(&ISSUES, &row).await?;
        register_id(&mut tx, "issue", &rec.id, &dst.id).await?;
        crate::posts::adopt::moved_issue(&mut tx, &rec.id, &dst.id, &dst.org_id).await?;
        // Counted in the new repo: nothing before, the milestone now.
        let mut before = old.clone();
        before.milestone_number = 0;
        before.repo_id = dst.id.clone();
        super::labels::track_milestone(&mut tx, Some(&before), &rec).await?;
        super::items::refresh_issue(&mut tx, &rec).await?;
        let mut cache = Cache::new(dst, &cx.actor_id).with_name(&src.id, &src.full_name);
        let issue = render(&mut tx, &mut cache, &rec, 1).await?;
        let payload = |dir: &str| {
            serde_json::json!({
                "direction": dir,
                "from": {"repo_id": src.id, "full_name": src.full_name, "number": number},
                "to": {"repo_id": dst.id, "full_name": dst.full_name, "number": new_number},
                "issue": serde_json::from_str::<serde_json::Value>(&pjson!(&issue)).unwrap_or_default(),
            })
            .to_string()
        };
        for (repo, dir) in [(src, "from"), (dst, "to")] {
            tx.emit_event(bee_proto::v1::Event {
                kind: "issue.transferred".into(),
                repo_id: repo.id.clone(),
                org_id: repo.org_id.clone(),
                subject: Some(subject(SubjectKind::Issue, &dst.id, &rec.id)),
                payload_json: payload(dir),
                ..Default::default()
            })
            .await?;
        }
        tx.audit(
            crate::audit::AuditDraft::new("issue.transfer", &src.org_id)
                .repo_id(&src.id)
                .target(SubjectKind::Issue, &rec.id)
                .after(
                    &serde_json::json!({"from": src.full_name, "number": number, "to": dst.full_name, "new_number": new_number}).to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok((issue, number))
    }

    // ---- comments ----

    /// Adds a conversation comment, optionally closing or reopening in the same call.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_comment(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        body: &str,
        saved_reply_id: &str,
        close: bool,
        reopen: bool,
        effects: &TextEffects,
    ) -> Result<Comment> {
        if close && reopen {
            return Err(ForgeError::invalid(
                "conflicting_flags",
                "`close` and `reopen` cannot both be set",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        if rec.locked && !actor.triage {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Locked,
                "the conversation is locked",
            ));
        }
        if !saved_reply_id.is_empty()
            && tx
                .get(&SAVED, &cx.actor_id, saved_reply_id)
                .await?
                .is_none()
        {
            return Err(ForgeError::not_found(format!(
                "saved reply {saved_reply_id}"
            )));
        }
        let mut comment = add_comment_in_tx(&mut tx, repo, actor, &mut rec, body, effects).await?;
        let old = rec.clone();
        let me = actor.principal.clone();
        let mut state_kind = None;
        if close && is_open(&rec) {
            if !actor.triage && pid(&rec.author) != me.id {
                return Err(ForgeError::permission_denied(
                    "only the author or a maintainer may close",
                ));
            }
            rec.state = IssueState::Closed as i32;
            rec.state_reason = IssueStateReason::Completed as i32;
            rec.closed_ms = tx.now_ms();
            rec.closed_by = Some(me.clone());
            add_timeline(&mut tx, &rec.id, Tk::Closed, &me, |e| {
                e.state_reason = IssueStateReason::Completed as i32
            })
            .await?;
            state_kind = Some(true);
        } else if reopen && !is_open(&rec) {
            if !actor.triage && pid(&rec.author) != me.id {
                return Err(ForgeError::permission_denied(
                    "only the author or a maintainer may reopen",
                ));
            }
            rec.state = IssueState::Open as i32;
            rec.state_reason = IssueStateReason::Reopened as i32;
            rec.closed_ms = 0;
            rec.closed_by = None;
            rec.duplicate_of = None;
            add_timeline(&mut tx, &rec.id, Tk::Reopened, &me, |e| {
                e.state_reason = IssueStateReason::Reopened as i32
            })
            .await?;
            state_kind = Some(false);
        }
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        comment.reactions = Vec::new();
        record(
            &mut tx,
            Change {
                kind: "issue_comment.created",
                action: "issue_comment.create",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Comment,
                subject_id: &comment.id,
                payload: serde_json::json!({
                    "comment": serde_json::from_str::<serde_json::Value>(&pjson!(&comment)).unwrap_or_default(),
                    "issue": serde_json::from_str::<serde_json::Value>(&pjson!(&issue)).unwrap_or_default(),
                })
                .to_string(),
                after: serde_json::json!({"issue": rec.number, "comment": comment.id, "saved_reply": saved_reply_id}).to_string(),
            },
        )
        .await?;
        if let Some(closed) = state_kind {
            super::automation::on_issue_state(&mut tx, &rec, closed, None).await?;
            let (kind, action) = if closed {
                ("issue.closed", "issue.close")
            } else {
                ("issue.reopened", "issue.reopen")
            };
            record(
                &mut tx,
                org_scope_change(repo, kind, action, &rec, pjson!(&issue)),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(comment)
    }

    /// An issue's comments, oldest first.
    pub async fn list_comments(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
        since_ms: Option<u64>,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<Comment>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, false).await?;
        let mut q = Scan::scope(rec.id.clone(), limit + 1).after(cursor(&rec.id, after));
        if let Some(s) = since_ms {
            q = q.n1_min(s as i64);
            q.order = crate::store::Order::Id;
        }
        let rows = tx.scan(&COMMENTS, &q).await?;
        let (rows, next) = paged(rows, limit);
        let mut out = Vec::new();
        for r in rows {
            let mut c: Comment = r.decode()?;
            c.edited = c.edited || false;
            let mine = if c.reactions.is_empty() {
                BTreeSet::new()
            } else {
                viewer_reactions(&mut tx, &c.id, &cx.actor_id).await?
            };
            for g in &mut c.reactions {
                g.viewer_reacted = mine.contains(&g.content);
            }
            out.push(c);
        }
        Ok((out, next))
    }

    /// The issue timeline, oldest first.
    pub async fn list_timeline(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
        kinds: &[i32],
        limit: usize,
        after: &str,
    ) -> Result<TimelinePage> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_issue(&mut tx, &repo.id, number, false).await?;
        let cache = Cache::new(repo, &cx.actor_id);
        let mut events = Vec::new();
        let mut next = String::new();
        let mut cur = cursor(&rec.id, after);
        'outer: loop {
            let rows = tx
                .scan(
                    &TIMELINE,
                    &Scan::scope(rec.id.clone(), 200).after(cur.clone()),
                )
                .await?;
            let n = rows.len();
            for r in rows {
                cur = Some(Cursor::after(&r));
                let mut ev: IssueTimelineEvent = r.decode()?;
                if !kinds.is_empty() && !kinds.contains(&ev.kind) {
                    continue;
                }
                // Events stored before `mentioned_user` existed carry the user in `assignee`.
                if ev.kind == Tk::Mentioned as i32 && ev.mentioned_user.is_none() {
                    ev.mentioned_user = ev.assignee.clone();
                }
                if ev.kind == Tk::Commented as i32 {
                    let cid = ev
                        .comment
                        .as_ref()
                        .map(|c| c.id.clone())
                        .unwrap_or_default();
                    match tx.get_msg::<Comment>(&COMMENTS, &rec.id, &cid).await? {
                        Some((c, _)) => ev.comment = Some(c),
                        None => continue,
                    }
                }
                if let Some(s) = ev.related_issue.as_mut() {
                    let ph = placeholder(&rec.repo_id);
                    let real = cache.name(&rec.repo_id);
                    s.repo_full_name = s.repo_full_name.replace(&ph, &real);
                    s.url = s.url.replace(&ph, &real);
                }
                if events.len() == limit {
                    next = events
                        .last()
                        .map(|e: &IssueTimelineEvent| e.id.clone())
                        .unwrap_or_default();
                    break 'outer;
                }
                events.push(ev);
            }
            if n < 200 {
                break;
            }
        }
        Ok(TimelinePage { events, next })
    }

    // ---- comment records (CommentService on issue conversations) ----

    /// The `(repo_id, issue_id)` a comment id belongs to, if it is an issue comment.
    pub async fn comment_location(&self, cx: &Cx, id: &str) -> Result<Option<(String, String)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some(issue_id) = lookup_id(&mut tx, "comment", id).await? else {
            return Ok(None);
        };
        let repo_id = lookup_id(&mut tx, "issue", &issue_id)
            .await?
            .unwrap_or_default();
        Ok(Some((repo_id, issue_id)))
    }

    /// Reads one comment.
    pub async fn get_comment(&self, cx: &Cx, id: &str) -> Result<Comment> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let issue_id = lookup_id(&mut tx, "comment", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))?;
        let (mut c, _) = tx
            .get_msg::<Comment>(&COMMENTS, &issue_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))?;
        let mine = viewer_reactions(&mut tx, id, &cx.actor_id).await?;
        for g in &mut c.reactions {
            g.viewer_reacted = mine.contains(&g.content);
        }
        Ok(c)
    }

    /// Edits a comment, keeping the previous body in its history. The author and triagers
    /// may edit.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_comment(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        id: &str,
        body: &str,
        expected_version: u64,
        effects: &TextEffects,
    ) -> Result<Comment> {
        validate_body(body)?;
        if body.trim().is_empty() {
            return Err(ForgeError::missing("body"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let issue_id = lookup_id(&mut tx, "comment", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))?;
        let row = tx
            .get_for_update(&COMMENTS, &issue_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))?;
        let mut c: Comment = row.decode()?;
        check_version(expected_version, row.version)?;
        if !actor.triage && pid(&c.author) != actor.principal.id {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may edit a comment",
            ));
        }
        if c.body != body {
            let edit = CommentEdit {
                comment_id: id.to_string(),
                editor: Some(actor.principal.clone()),
                previous_body: c.body.clone(),
                edited_at: Some(ts(tx.now_ms())),
            };
            let eid = tx.new_id()?;
            tx.insert(&COMMENT_EDITS, &Row::new(id, eid).msg(&edit))
                .await?;
            c.body = body.to_string();
            c.edited = true;
            c.updated_at = Some(ts(tx.now_ms()));
            let new_row = Row::new(issue_id.clone(), id)
                .k1(row.k1.clone().unwrap_or_default())
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&c);
            tx.update(&COMMENTS, &new_row).await?;
            let (mut rec, irow) = load_issue_id(&mut tx, &issue_id, true)
                .await?
                .ok_or_else(|| ForgeError::not_found("issue"))?;
            let old = rec.clone();
            let src = subject(SubjectKind::Comment, &repo.id, id);
            apply_effects(&mut tx, repo, &mut rec, src, &actor.principal, effects).await?;
            if rec != old {
                save_issue(&mut tx, Some(&old), &mut rec, irow.version).await?;
            }
            record(
                &mut tx,
                Change {
                    kind: "issue_comment.edited",
                    action: "issue_comment.update",
                    org_id: &repo.org_id,
                    repo_id: &repo.id,
                    subject_kind: SubjectKind::Comment,
                    subject_id: id,
                    payload: pjson!(&c),
                    after: serde_json::json!({"comment": id}).to_string(),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(c)
    }

    /// The edit history of a comment, oldest first.
    pub async fn list_comment_edits(
        &self,
        cx: &Cx,
        id: &str,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<CommentEdit>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if lookup_id(&mut tx, "comment", id).await?.is_none() {
            return Err(ForgeError::not_found(format!("comment {id}")));
        }
        let rows = tx
            .scan(
                &COMMENT_EDITS,
                &Scan::scope(id, limit + 1).after(cursor(id, after)),
            )
            .await?;
        let (rows, next) = paged(rows, limit);
        Ok((
            rows.iter()
                .map(|r| r.decode())
                .collect::<Result<Vec<CommentEdit>>>()?,
            next,
        ))
    }

    /// Deletes a comment (the author or a triager); its edits and reactions go with it.
    pub async fn delete_comment(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let issue_id = lookup_id(&mut tx, "comment", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))?;
        let (c, _) = tx
            .get_msg::<Comment>(&COMMENTS, &issue_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {id}")))?;
        if !actor.triage && pid(&c.author) != actor.principal.id {
            return Err(ForgeError::permission_denied(
                "only the author or a maintainer may delete a comment",
            ));
        }
        purge_comment(&mut tx, &issue_id, id).await?;
        let (mut rec, row) = load_issue_id(&mut tx, &issue_id, true)
            .await?
            .ok_or_else(|| ForgeError::not_found("issue"))?;
        let old = rec.clone();
        rec.comment_count = rec.comment_count.saturating_sub(1);
        add_timeline(
            &mut tx,
            &issue_id,
            Tk::CommentDeleted,
            &actor.principal,
            |_| {},
        )
        .await?;
        save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        record(
            &mut tx,
            Change {
                kind: "issue_comment.deleted",
                action: "issue_comment.delete",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Comment,
                subject_id: id,
                payload: serde_json::json!({"id": id, "issue_id": issue_id}).to_string(),
                after: serde_json::json!({"comment": id}).to_string(),
            },
        )
        .await?;
        tx.commit().await
    }

    // ---- reactions ----

    /// Adds the caller's reaction to an issue or a comment; adding it again returns the
    /// existing one.
    pub async fn add_reaction(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        subject_id: &str,
        content: ReactionContent,
    ) -> Result<Reaction> {
        if content == ReactionContent::Unspecified {
            return Err(ForgeError::missing("content"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rid = format!("{}:{}", actor.principal.id, content as i32);
        if let Some((r, _)) = tx.get_msg::<Reaction>(&REACTIONS, subject_id, &rid).await? {
            return Ok(r);
        }
        let reaction = Reaction {
            id: tx.new_id()?,
            content: content as i32,
            user: Some(actor.principal.clone()),
            created_at: Some(ts(tx.now_ms())),
        };
        adjust_reaction(&mut tx, subject_id, content as i32, 1).await?;
        tx.insert(
            &REACTIONS,
            &Row::new(subject_id, rid)
                .k1(actor.principal.id.clone())
                .msg(&reaction),
        )
        .await?;
        record(
            &mut tx,
            Change {
                kind: "issue_comment.reacted",
                action: "issue_comment.react",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id,
                payload: serde_json::json!({"subject": subject_id, "content": content as i32, "added": true}).to_string(),
                after: serde_json::json!({"subject": subject_id, "content": content.as_str_name()}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(reaction)
    }

    /// Removes the caller's reaction.
    pub async fn remove_reaction(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        subject_id: &str,
        content: ReactionContent,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rid = format!("{}:{}", actor.principal.id, content as i32);
        if !tx.delete(&REACTIONS, subject_id, &rid).await? {
            return Ok(());
        }
        adjust_reaction(&mut tx, subject_id, content as i32, -1).await?;
        record(
            &mut tx,
            Change {
                kind: "issue_comment.reacted",
                action: "issue_comment.unreact",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id,
                payload: serde_json::json!({"subject": subject_id, "content": content as i32, "added": false}).to_string(),
                after: serde_json::json!({"subject": subject_id, "content": content.as_str_name()}).to_string(),
            },
        )
        .await?;
        tx.commit().await
    }

    /// Reactions on an issue or comment, with the per-content groups.
    pub async fn list_reactions(
        &self,
        cx: &Cx,
        subject_id: &str,
        content: ReactionContent,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<Reaction>, Vec<ReactionGroup>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all =
            scan_all_msgs::<Reaction>(&mut tx, &REACTIONS, Scan::scope(subject_id, 1)).await?;
        let mut counts: BTreeMap<i32, (u32, bool)> = BTreeMap::new();
        for (r, _) in &all {
            let e = counts.entry(r.content).or_default();
            e.0 += 1;
            e.1 |= r.user.as_ref().is_some_and(|u| u.id == cx.actor_id);
        }
        let groups = counts
            .into_iter()
            .map(|(c, (n, v))| ReactionGroup {
                content: c,
                count: n,
                viewer_reacted: v,
            })
            .collect();
        let mut listed: Vec<Reaction> = all
            .into_iter()
            .map(|(r, _)| r)
            .filter(|r| content == ReactionContent::Unspecified || r.content == content as i32)
            .collect();
        listed.sort_by(|a, b| a.id.cmp(&b.id));
        let rest: Vec<Reaction> = listed
            .into_iter()
            .filter(|r| after.is_empty() || r.id.as_str() > after)
            .collect();
        let next = if rest.len() > limit {
            rest[limit - 1].id.clone()
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), groups, next))
    }

    /// Whether `id` is an issue or an issue comment held here, and its repo.
    pub async fn reaction_subject_repo(&self, cx: &Cx, id: &str) -> Result<Option<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some(r) = lookup_id(&mut tx, "issue", id).await? {
            return Ok(Some(r));
        }
        if let Some(issue_id) = lookup_id(&mut tx, "comment", id).await? {
            return lookup_id(&mut tx, "issue", &issue_id).await;
        }
        Ok(None)
    }
}

async fn lookup_label(tx: &mut Tx, repo_id: &str, id: &str) -> Result<Option<Label>> {
    Ok(tx
        .get_msg::<Label>(&LABELS, repo_id, id)
        .await?
        .map(|(l, _)| l))
}

async fn type_name(tx: &mut Tx, org_id: &str, id: &str) -> Result<String> {
    Ok(tx
        .get_msg::<bee_proto::v1::IssueType>(&TYPES, org_id, id)
        .await?
        .map(|(t, _)| t.name)
        .unwrap_or_default())
}

async fn adjust_reaction(tx: &mut Tx, subject_id: &str, content: i32, delta: i64) -> Result<()> {
    fn bump(counts: &mut Vec<ReactionCount>, content: i32, delta: i64) {
        match counts.iter_mut().find(|r| r.content == content) {
            Some(r) => r.count = (i64::from(r.count) + delta).max(0) as u32,
            None if delta > 0 => counts.push(ReactionCount {
                content,
                count: delta as u32,
            }),
            None => {}
        }
        counts.retain(|r| r.count > 0);
    }
    if let Some((mut rec, row)) = load_issue_id(tx, subject_id, true).await? {
        let old = rec.clone();
        bump(&mut rec.reactions, content, delta);
        rec.reaction_total = rec.reactions.iter().map(|r| r.count).sum();
        save_issue(tx, Some(&old), &mut rec, row.version).await?;
        return Ok(());
    }
    let Some(issue_id) = lookup_id(tx, "comment", subject_id).await? else {
        return Err(ForgeError::not_found(format!(
            "reaction subject {subject_id}"
        )));
    };
    let row = tx
        .get_for_update(&COMMENTS, &issue_id, subject_id)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("comment {subject_id}")))?;
    let mut c: Comment = row.decode()?;
    let mut counts: Vec<ReactionCount> = c
        .reactions
        .iter()
        .map(|g| ReactionCount {
            content: g.content,
            count: g.count,
        })
        .collect();
    bump(&mut counts, content, delta);
    c.reactions = counts
        .iter()
        .map(|r| ReactionGroup {
            content: r.content,
            count: r.count,
            viewer_reacted: false,
        })
        .collect();
    let new_row = Row::new(issue_id, subject_id)
        .k1(row.k1.clone().unwrap_or_default())
        .n1(row.n1.unwrap_or(0))
        .version(row.version)
        .msg(&c);
    tx.update(&COMMENTS, &new_row).await?;
    Ok(())
}

async fn purge_comment(tx: &mut Tx, issue_id: &str, id: &str) -> Result<()> {
    tx.delete(&COMMENTS, issue_id, id).await?;
    forget_id(tx, "comment", id).await?;
    for r in scan_all(tx, &COMMENT_EDITS, Scan::scope(id, 1)).await? {
        tx.delete(&COMMENT_EDITS, id, &r.id).await?;
    }
    for r in scan_all(tx, &REACTIONS, Scan::scope(id, 1)).await? {
        tx.delete(&REACTIONS, id, &r.id).await?;
    }
    Ok(())
}

/// Adds a comment to `rec` (already loaded for update) and applies the text effects. The
/// issue row is written here so the counters stay right; the caller may write it again.
async fn add_comment_in_tx(
    tx: &mut Tx,
    repo: &RepoInfo,
    actor: &Actor,
    rec: &mut IssueRec,
    body: &str,
    effects: &TextEffects,
) -> Result<Comment> {
    validate_body(body)?;
    if body.trim().is_empty() {
        return Err(ForgeError::missing("body"));
    }
    let id = tx.new_id()?;
    let now = tx.now_ms();
    let comment = Comment {
        id: id.clone(),
        subject: Some(subject(SubjectKind::Issue, &repo.id, &rec.id)),
        author: Some(actor.principal.clone()),
        body: body.to_string(),
        created_at: Some(ts(now)),
        updated_at: Some(ts(now)),
        author_association: actor.association.clone(),
        url: format!(
            "/{}/issues/{}#issuecomment-{id}",
            repo.full_name, rec.number
        ),
        ..Default::default()
    };
    tx.insert(
        &COMMENTS,
        &Row::new(rec.id.clone(), id.clone())
            .k1(actor.principal.id.clone())
            .n1(now as i64)
            .msg(&comment),
    )
    .await?;
    register_id(tx, "comment", &id, &rec.id).await?;
    rec.comment_count += 1;
    if !rec.commenter_ids.contains(&actor.principal.id) {
        rec.commenter_ids.push(actor.principal.id.clone());
    }
    let cid = id.clone();
    add_timeline(tx, &rec.id, Tk::Commented, &actor.principal, |e| {
        e.comment = Some(Comment {
            id: cid,
            ..Default::default()
        });
    })
    .await?;
    let src = subject(SubjectKind::Comment, &repo.id, &id);
    apply_effects(tx, repo, rec, src, &actor.principal, effects).await?;
    Ok(comment)
}
