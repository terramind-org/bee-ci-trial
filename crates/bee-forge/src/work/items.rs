//! Project items: issues, change requests and drafts with typed field values.

use bee_proto::v1::{
    IssueState, IssueTimelineEventKind as Tk, Principal, Project, ProjectField, ProjectFieldKind,
    ProjectFieldValue, ProjectItem, ProjectItemKind, ProjectView, SubjectKind,
    project_field_value::Value,
};

use super::issues::{
    Cache, IssueTarget, NewIssue, TextEffects, add_timeline, load_issue_id, load_target,
};
use super::model::{IssueRec, ItemRec, log_kind};
use super::pfilter::{Compiled, EvalCtx};
use super::projects::{
    automation_principal, load_fields, load_project, log_item, org_of, pchange, save_project,
};
use super::relations::ChangeLink;
use super::*;
use crate::ids::{ts, ts_ms};
use std::collections::BTreeSet;

/// First manual-order position; moves to the top subtract from the smallest, so positions
/// stay positive for any realistic number of moves.
const BASE: i64 = 1_000_000_000_000;
const GAP: i64 = 1000;

/// What the issue-side data of an item looks like at one moment.
pub(crate) struct Snapshot {
    pub title: String,
    pub state: &'static str,
    pub assignees: Vec<Principal>,
    pub labels: Vec<String>,
    pub milestone: String,
    pub issue_type: String,
    pub parent: String,
    pub linked_changes: u32,
    pub sub_percent: u32,
    pub repo_id: String,
    pub author_id: String,
    pub number: u64,
}

pub(crate) fn pos_str(n: i64) -> String {
    format!("{:016}", n.max(0))
}

fn pos_of(s: &str) -> i64 {
    s.parse().unwrap_or(BASE)
}

/// Loads an item.
pub(crate) async fn load_item(
    tx: &mut Tx,
    project_id: &str,
    item_id: &str,
    lock: bool,
) -> Result<(ItemRec, Row)> {
    let row = if lock {
        tx.get_for_update(&PROJECT_ITEMS, project_id, item_id)
            .await?
    } else {
        tx.get(&PROJECT_ITEMS, project_id, item_id).await?
    }
    .ok_or_else(|| ForgeError::not_found(format!("project item {item_id}")))?;
    let mut rec: ItemRec = row.decode()?;
    if let Some(i) = rec.item.as_mut() {
        i.version = row.version;
    }
    Ok((rec, row))
}

/// Writes an item (insert at version 0); the update time moves unless `touch` is false.
pub(crate) async fn save_item(
    tx: &mut Tx,
    project_id: &str,
    rec: &mut ItemRec,
    version: u64,
    touch: bool,
) -> Result<u64> {
    let (id, pos) = {
        let it = rec
            .item
            .as_mut()
            .ok_or_else(|| ForgeError::internal("item without body"))?;
        if touch {
            it.updated_at = Some(ts(tx.now_ms()));
        }
        (it.id.clone(), pos_of(&it.position))
    };
    let mut row = Row::new(project_id, id).n1(pos).version(version).msg(rec);
    if !rec.content_key.is_empty() {
        row = row.k1(rec.content_key.clone());
    }
    if version == 0 {
        tx.insert(&PROJECT_ITEMS, &row).await?;
        Ok(1)
    } else {
        tx.update(&PROJECT_ITEMS, &row).await
    }
}

async fn end_position(tx: &mut Tx, project_id: &str) -> Result<i64> {
    let last = tx
        .scan(&PROJECT_ITEMS, &Scan::scope(project_id, 1).by_n1().desc())
        .await?;
    Ok(last
        .first()
        .and_then(|r| r.n1)
        .map(|n| n + GAP)
        .unwrap_or(BASE))
}

/// The snapshot of an issue for item display and filters.
pub(crate) async fn snapshot_issue(tx: &mut Tx, rec: &IssueRec) -> Result<Snapshot> {
    let repo = RepoInfo {
        id: rec.repo_id.clone(),
        org_id: rec.org_id.clone(),
        full_name: placeholder(&rec.repo_id),
    };
    let mut cache = Cache::new(&repo, "");
    let labels_map = cache.labels(tx, &rec.repo_id).await?.clone();
    let labels = rec
        .label_ids
        .iter()
        .filter_map(|i| labels_map.get(i).map(|l| l.name.clone()))
        .collect();
    let milestone = if rec.milestone_number > 0 {
        cache
            .milestones(tx, &rec.repo_id)
            .await?
            .get(&rec.milestone_number)
            .map(|m| m.title.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let issue_type = if rec.issue_type_id.is_empty() {
        String::new()
    } else {
        cache
            .types(tx, &rec.org_id)
            .await?
            .get(&rec.issue_type_id)
            .map(|t| t.name.clone())
            .unwrap_or_default()
    };
    let parent = if rec.parent_id.is_empty() {
        String::new()
    } else {
        load_issue_id(tx, &rec.parent_id, false)
            .await?
            .map(|(p, _)| p.title)
            .unwrap_or_default()
    };
    let sub_percent = if rec.child_count > 0 {
        super::relations::sub_progress(tx, &rec.id)
            .await?
            .percent_completed
    } else {
        0
    };
    Ok(Snapshot {
        title: rec.title.clone(),
        state: if rec.state == IssueState::Closed as i32 {
            "closed"
        } else {
            "open"
        },
        assignees: rec.assignees.clone(),
        labels,
        milestone,
        issue_type,
        parent,
        linked_changes: rec.linked_change_ids.len() as u32,
        sub_percent,
        repo_id: rec.repo_id.clone(),
        author_id: rec
            .author
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_default(),
        number: rec.number,
    })
}

/// Applies a snapshot to an item; returns whether anything changed.
fn apply_snapshot(rec: &mut ItemRec, s: &Snapshot) -> bool {
    let before = rec.clone();
    if let Some(it) = rec.item.as_mut() {
        it.title = s.title.clone();
        it.content_state = s.state.to_string();
        it.assignees = s.assignees.clone();
        it.repo_full_name = placeholder(&s.repo_id);
        it.number = s.number;
    }
    rec.labels = s.labels.clone();
    rec.milestone = s.milestone.clone();
    rec.issue_type = s.issue_type.clone();
    rec.parent = s.parent.clone();
    rec.linked_changes = s.linked_changes;
    rec.sub_percent = s.sub_percent;
    rec.repo_id = s.repo_id.clone();
    rec.author_id = s.author_id.clone();
    *rec != before
}

/// Refreshes the snapshots of the items that show this issue.
pub(crate) async fn refresh_issue(tx: &mut Tx, rec: &IssueRec) -> Result<()> {
    let key = format!("issue:{}", rec.id);
    let idx = tx.scan(&ITEM_INDEX, &Scan::scope(key, 100)).await?;
    if idx.is_empty() {
        return Ok(());
    }
    let snap = snapshot_issue(tx, rec).await?;
    for r in idx {
        let Some(project_id) = r.k1.clone() else {
            continue;
        };
        let (mut item, row) = load_item(tx, &project_id, &r.id, true).await?;
        if apply_snapshot(&mut item, &snap) {
            save_item(tx, &project_id, &mut item, row.version, true).await?;
        }
    }
    Ok(())
}

/// Updates the items that show a change request.
pub(crate) async fn refresh_change(
    tx: &mut Tx,
    change: &ChangeLink,
    author_id: &str,
    reviewers: &[String],
) -> Result<()> {
    let key = format!("cr:{}", change.id);
    for r in tx.scan(&ITEM_INDEX, &Scan::scope(key, 100)).await? {
        let Some(project_id) = r.k1.clone() else {
            continue;
        };
        let (mut item, row) = load_item(tx, &project_id, &r.id, true).await?;
        let before = item.clone();
        if let Some(it) = item.item.as_mut() {
            if !change.title.is_empty() {
                it.title = change.title.clone();
            }
            it.content_state = change.state.clone();
            it.number = change.number;
            it.repo_full_name = placeholder(&change.repo_id);
        }
        item.repo_id = change.repo_id.clone();
        if !author_id.is_empty() {
            item.author_id = author_id.to_string();
        }
        item.reviewers = reviewers.to_vec();
        if item != before {
            save_item(tx, &project_id, &mut item, row.version, true).await?;
        }
    }
    Ok(())
}

/// Display text of a field value.
pub(crate) fn display_of(f: &ProjectField, v: &ProjectFieldValue) -> String {
    match &v.value {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Number(n)) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Some(Value::DateOn(d)) => super::fields::iso_date(ts_ms(d)),
        Some(Value::OptionId(o)) => f
            .options
            .iter()
            .find(|x| x.id == *o)
            .map(|x| x.name.clone())
            .unwrap_or_default(),
        Some(Value::IterationId(i)) => {
            let cfg = f.iteration_config.clone().unwrap_or_default();
            cfg.iterations
                .iter()
                .chain(cfg.completed_iterations.iter())
                .find(|x| x.id == *i)
                .map(|x| x.title.clone())
                .unwrap_or_default()
        }
        None => String::new(),
    }
}

/// The wire form of an item with every field value filled in.
pub(crate) fn render_item(rec: &ItemRec, fields: &[ProjectField], only: &[String]) -> ProjectItem {
    let mut item = rec.item.clone().unwrap_or_default();
    let want = |f: &ProjectField| only.is_empty() || only.contains(&f.id);
    let mut out: Vec<ProjectFieldValue> = Vec::new();
    for f in fields.iter().filter(|f| want(f)) {
        if f.built_in {
            let display =
                match ProjectFieldKind::try_from(f.kind).unwrap_or(ProjectFieldKind::Unspecified) {
                    ProjectFieldKind::Title => item.title.clone(),
                    ProjectFieldKind::Assignees => item
                        .assignees
                        .iter()
                        .map(|a| {
                            if a.display.is_empty() {
                                a.id.clone()
                            } else {
                                a.display.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    ProjectFieldKind::Labels => rec.labels.join(", "),
                    ProjectFieldKind::Milestone => rec.milestone.clone(),
                    ProjectFieldKind::Repository => item.repo_full_name.clone(),
                    ProjectFieldKind::LinkedChanges => {
                        if rec.linked_changes > 0 {
                            rec.linked_changes.to_string()
                        } else {
                            String::new()
                        }
                    }
                    ProjectFieldKind::Reviewers => rec.reviewers.join(", "),
                    ProjectFieldKind::ParentIssue => rec.parent.clone(),
                    ProjectFieldKind::SubIssuesProgress => {
                        if rec.sub_percent > 0 {
                            format!("{}%", rec.sub_percent)
                        } else {
                            String::new()
                        }
                    }
                    ProjectFieldKind::IssueType => rec.issue_type.clone(),
                    _ => String::new(),
                };
            if !display.is_empty() {
                out.push(ProjectFieldValue {
                    field_id: f.id.clone(),
                    field_name: f.name.clone(),
                    kind: f.kind,
                    value: Some(Value::Text(display.clone())),
                    display_value: display,
                });
            }
        } else if let Some(v) = item.field_values.iter().find(|v| v.field_id == f.id) {
            let mut v = v.clone();
            v.field_name = f.name.clone();
            v.kind = f.kind;
            v.display_value = display_of(f, &v);
            out.push(v);
        }
    }
    item.field_values = out;
    item
}

#[allow(clippy::too_many_arguments)]
fn new_item(
    id: String,
    project_id: &str,
    kind: ProjectItemKind,
    title: &str,
    state: &str,
    position: i64,
    creator: &Principal,
    now: u64,
) -> ItemRec {
    ItemRec {
        item: Some(ProjectItem {
            id,
            project_id: project_id.to_string(),
            kind: kind as i32,
            title: title.to_string(),
            content_state: state.to_string(),
            position: pos_str(position),
            creator: Some(creator.clone()),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            version: 1,
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn item_event(
    tx: &mut Tx,
    p: &Project,
    action: &str,
    audit: &str,
    item: &ProjectItem,
) -> Result<()> {
    let org = org_of(p);
    record(
        tx,
        pchange(
            "project_item.changed",
            audit,
            &org,
            p,
            serde_json::json!({
                "action": action,
                "project_id": p.id,
                "item": serde_json::from_str::<serde_json::Value>(&pjson!(item)).unwrap_or_default(),
            })
            .to_string(),
            serde_json::json!({"project": p.number, "item": item.id, "kind": item.kind}).to_string(),
        ),
    )
    .await
}

fn open_count_delta(rec: &ItemRec) -> i64 {
    if rec.item.as_ref().is_some_and(|i| i.archived) {
        0
    } else {
        1
    }
}

/// Adds an issue to a project (workflows included). The service has checked who may.
pub(crate) async fn add_issue_in_tx(
    tx: &mut Tx,
    _repo: &RepoInfo,
    project_id: &str,
    issue: &IssueRec,
    actor: &Principal,
) -> Result<ProjectItem> {
    let (mut p, prow) = load_project(tx, project_id, true).await?;
    if p.closed {
        return Err(ForgeError::invalid_state("the project is closed"));
    }
    let key = format!("issue:{}", issue.id);
    if !tx
        .scan(
            &PROJECT_ITEMS,
            &Scan::scope(project_id, 1).key(Key::K1(key.clone())),
        )
        .await?
        .is_empty()
    {
        return Err(ForgeError::already_exists("project item"));
    }
    if p.item_count as usize >= limits::PROJECT_ITEMS {
        return Err(exhausted(
            "project_items",
            limits::PROJECT_ITEMS,
            p.item_count as usize + 1,
        ));
    }
    let snap = snapshot_issue(tx, issue).await?;
    let pos = end_position(tx, project_id).await?;
    let id = tx.new_id()?;
    let mut rec = new_item(
        id.clone(),
        project_id,
        ProjectItemKind::Issue,
        &issue.title,
        snap.state,
        pos,
        actor,
        tx.now_ms(),
    );
    apply_snapshot(&mut rec, &snap);
    rec.content_key = key.clone();
    if let Some(it) = rec.item.as_mut() {
        it.content = Some(subject(SubjectKind::Issue, &issue.repo_id, &issue.id));
    }
    save_item(tx, project_id, &mut rec, 0, false).await?;
    tx.insert(&ITEM_INDEX, &Row::new(key, id.clone()).k1(project_id))
        .await?;
    p.item_count += 1;
    save_project(tx, &mut p, prow.version).await?;
    log_item(tx, project_id, &id, log_kind::ADDED, "", "", 0.0).await?;
    let pid = project_id.to_string();
    add_timeline(tx, &issue.id, Tk::AddedToProject, actor, |e| {
        e.project_id = pid
    })
    .await?;
    super::automation::on_item_added(tx, &p, &id).await?;
    let (rec, _) = load_item(tx, project_id, &id, false).await?;
    let fields = load_fields(tx, project_id).await?;
    let item = render_item(&rec, &fields, &[]);
    item_event(tx, &p, "added", "project_item.add", &item).await?;
    Ok(item)
}

/// Adds a change request to a project.
pub(crate) async fn add_change_in_tx(
    tx: &mut Tx,
    project_id: &str,
    change: &ChangeLink,
    author_id: &str,
    actor: &Principal,
) -> Result<ProjectItem> {
    let (mut p, prow) = load_project(tx, project_id, true).await?;
    if p.closed {
        return Err(ForgeError::invalid_state("the project is closed"));
    }
    let key = format!("cr:{}", change.id);
    if !tx
        .scan(
            &PROJECT_ITEMS,
            &Scan::scope(project_id, 1).key(Key::K1(key.clone())),
        )
        .await?
        .is_empty()
    {
        return Err(ForgeError::already_exists("project item"));
    }
    if p.item_count as usize >= limits::PROJECT_ITEMS {
        return Err(exhausted(
            "project_items",
            limits::PROJECT_ITEMS,
            p.item_count as usize + 1,
        ));
    }
    let pos = end_position(tx, project_id).await?;
    let id = tx.new_id()?;
    let state = if change.state.is_empty() {
        "open"
    } else {
        change.state.as_str()
    };
    let mut rec = new_item(
        id.clone(),
        project_id,
        ProjectItemKind::ChangeRequest,
        &change.title,
        state,
        pos,
        actor,
        tx.now_ms(),
    );
    rec.content_key = key.clone();
    rec.repo_id = change.repo_id.clone();
    rec.author_id = author_id.to_string();
    if let Some(it) = rec.item.as_mut() {
        it.number = change.number;
        it.repo_full_name = placeholder(&change.repo_id);
        it.content = Some(subject(
            SubjectKind::ChangeRequest,
            &change.repo_id,
            &change.id,
        ));
    }
    save_item(tx, project_id, &mut rec, 0, false).await?;
    tx.insert(&ITEM_INDEX, &Row::new(key, id.clone()).k1(project_id))
        .await?;
    p.item_count += 1;
    save_project(tx, &mut p, prow.version).await?;
    log_item(tx, project_id, &id, log_kind::ADDED, "", "", 0.0).await?;
    super::automation::on_item_added(tx, &p, &id).await?;
    let (rec, _) = load_item(tx, project_id, &id, false).await?;
    let fields = load_fields(tx, project_id).await?;
    let item = render_item(&rec, &fields, &[]);
    item_event(tx, &p, "added", "project_item.add", &item).await?;
    Ok(item)
}

/// Deletes an item's rows (item and index) and, for issues, notes it on their timeline.
pub(crate) async fn drop_item_rows(
    tx: &mut Tx,
    _p: &Project,
    it: &ItemRec,
    item_id: &str,
    actor: &Principal,
    timeline: bool,
) -> Result<()> {
    let project_id = it
        .item
        .as_ref()
        .map(|i| i.project_id.clone())
        .unwrap_or_default();
    tx.delete(&PROJECT_ITEMS, &project_id, item_id).await?;
    if !it.content_key.is_empty() {
        tx.delete(&ITEM_INDEX, &it.content_key, item_id).await?;
        if timeline && let Some(issue_id) = it.content_key.strip_prefix("issue:") {
            let pid = project_id.clone();
            if load_issue_id(tx, issue_id, false).await?.is_some() {
                add_timeline(tx, issue_id, Tk::RemovedFromProject, actor, |e| {
                    e.project_id = pid
                })
                .await?;
            }
        }
    }
    Ok(())
}

/// The content was deleted: its items go.
pub(crate) async fn remove_content(tx: &mut Tx, content_key: &str) -> Result<()> {
    for r in scan_all(tx, &ITEM_INDEX, Scan::scope(content_key, 1)).await? {
        let Some(project_id) = r.k1.clone() else {
            continue;
        };
        let Ok((mut p, prow)) = load_project(tx, &project_id, true).await else {
            continue;
        };
        if let Ok((it, _)) = load_item(tx, &project_id, &r.id, true).await {
            let archived = it.item.as_ref().is_some_and(|i| i.archived);
            drop_item_rows(tx, &p, &it, &r.id, &automation_principal(), false).await?;
            if !archived {
                p.item_count = p.item_count.saturating_sub(1);
            }
            save_project(tx, &mut p, prow.version).await?;
            log_item(tx, &project_id, &r.id, log_kind::REMOVED, "", "", 0.0).await?;
        }
    }
    Ok(())
}

/// Copies a project's draft items (with their values) into a new project.
pub(crate) async fn copy_drafts(
    tx: &mut Tx,
    src_id: &str,
    dst: &mut Project,
    map: &super::views::IdMap,
    actor: &Principal,
) -> Result<()> {
    let mut rows =
        scan_all_msgs::<ItemRec>(tx, &PROJECT_ITEMS, Scan::scope(src_id, 1).by_n1()).await?;
    rows.retain(|(r, _)| r.content_key.is_empty());
    for (mut rec, _) in rows {
        let id = tx.new_id()?;
        if let Some(it) = rec.item.as_mut() {
            it.id = id.clone();
            it.project_id = dst.id.clone();
            it.creator = Some(actor.clone());
            it.created_at = Some(ts(tx.now_ms()));
            it.updated_at = Some(ts(tx.now_ms()));
            it.archived = false;
            it.archived_at = None;
            for v in &mut it.field_values {
                v.field_id = map.field(&v.field_id);
                match &mut v.value {
                    Some(Value::OptionId(o)) => *o = map.id(o),
                    Some(Value::IterationId(i)) => *i = map.id(i),
                    _ => {}
                }
            }
        }
        save_item(tx, &dst.id, &mut rec, 0, false).await?;
        dst.item_count += 1;
        log_item(tx, &dst.id, &id, log_kind::ADDED, "", "", 0.0).await?;
    }
    Ok(())
}

/// Item list options.
#[derive(Clone, Debug, Default)]
pub struct ItemQuery {
    /// Filter text.
    pub query: String,
    /// A saved view whose filter and sort apply first.
    pub view_id: String,
    /// Include archived items.
    pub include_archived: bool,
    /// Only archived items.
    pub archived_only: bool,
    /// Field ids whose values are returned (empty = all).
    pub field_ids: Vec<String>,
    /// Resolved principals, repos and the clock.
    pub ctx: EvalCtx,
    /// Ids of repos whose content the caller cannot read. Their items are returned as redacted
    /// placeholders by the service; when a filter or a sort applies they are left out instead,
    /// so neither a match nor an order says anything about content the caller cannot see.
    pub hidden_repos: BTreeSet<String>,
}

/// The repo id of an item's content (issue or change request); `None` for drafts.
fn item_repo(r: &ItemRec) -> Option<&str> {
    r.item
        .as_ref()
        .and_then(|i| i.content.as_ref())
        .and_then(|c| c.repo.as_ref())
        .and_then(|r| match &r.repo {
            Some(repo_ref::Repo::Id(id)) => Some(id.as_str()),
            _ => None,
        })
}

/// A content reference to add.
#[derive(Clone, Debug)]
pub enum ItemContent {
    /// An issue.
    Issue(IssueTarget),
    /// A change request.
    Change {
        /// The change request.
        change: ChangeLink,
        /// Its author id.
        author_id: String,
    },
}

#[derive(PartialEq, PartialOrd)]
enum SortVal {
    Num(f64),
    Text(String),
    Missing,
}

fn sort_val(rec: &ItemRec, f: &ProjectField) -> SortVal {
    let it = rec.item.clone().unwrap_or_default();
    if f.built_in {
        let s = match ProjectFieldKind::try_from(f.kind).unwrap_or(ProjectFieldKind::Unspecified) {
            ProjectFieldKind::Title => it.title.to_lowercase(),
            ProjectFieldKind::Assignees => it
                .assignees
                .first()
                .map(|a| a.display.to_lowercase())
                .unwrap_or_default(),
            ProjectFieldKind::Labels => rec
                .labels
                .first()
                .map(|l| l.to_lowercase())
                .unwrap_or_default(),
            ProjectFieldKind::Milestone => rec.milestone.to_lowercase(),
            ProjectFieldKind::Repository => it.repo_full_name.to_lowercase(),
            ProjectFieldKind::IssueType => rec.issue_type.to_lowercase(),
            ProjectFieldKind::ParentIssue => rec.parent.to_lowercase(),
            ProjectFieldKind::LinkedChanges => return SortVal::Num(f64::from(rec.linked_changes)),
            ProjectFieldKind::SubIssuesProgress => return SortVal::Num(f64::from(rec.sub_percent)),
            _ => String::new(),
        };
        return if s.is_empty() {
            SortVal::Missing
        } else {
            SortVal::Text(s)
        };
    }
    let Some(v) = it.field_values.iter().find(|v| v.field_id == f.id) else {
        return SortVal::Missing;
    };
    match &v.value {
        Some(Value::Number(n)) => SortVal::Num(*n),
        Some(Value::DateOn(d)) => SortVal::Num(ts_ms(d) as f64),
        Some(Value::Text(t)) => SortVal::Text(t.to_lowercase()),
        Some(Value::OptionId(o)) => f
            .options
            .iter()
            .position(|x| x.id == *o)
            .map(|i| SortVal::Num(i as f64))
            .unwrap_or(SortVal::Missing),
        Some(Value::IterationId(i)) => {
            let cfg = f.iteration_config.clone().unwrap_or_default();
            cfg.iterations
                .iter()
                .chain(cfg.completed_iterations.iter())
                .find(|x| x.id == *i)
                .map(|x| SortVal::Num(x.start_on.as_ref().map(ts_ms).unwrap_or(0) as f64))
                .unwrap_or(SortVal::Missing)
        }
        None => SortVal::Missing,
    }
}

fn cmp_val(a: &SortVal, b: &SortVal, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (SortVal::Missing, SortVal::Missing) => Equal,
        // Items without a value always sort last.
        (SortVal::Missing, _) => Greater,
        (_, SortVal::Missing) => Less,
        _ => {
            let o = a.partial_cmp(b).unwrap_or(Equal);
            if desc { o.reverse() } else { o }
        }
    }
}

/// Orders items by a view's sort keys, then manual position.
fn sort_items(
    items: &mut [ItemRec],
    fields: &[ProjectField],
    sort: &[bee_proto::v1::ProjectViewSort],
) {
    items.sort_by(|a, b| {
        for s in sort {
            let Some(f) = fields.iter().find(|f| f.id == s.field_id) else {
                continue;
            };
            let o = cmp_val(&sort_val(a, f), &sort_val(b, f), s.descending);
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        let pa = a
            .item
            .as_ref()
            .map(|i| i.position.clone())
            .unwrap_or_default();
        let pb = b
            .item
            .as_ref()
            .map(|i| i.position.clone())
            .unwrap_or_default();
        pa.cmp(&pb)
    });
}

/// Archives or restores an item inside a transaction.
pub(crate) async fn set_archived_in_tx(
    tx: &mut Tx,
    project_id: &str,
    item_id: &str,
    archive: bool,
) -> Result<ProjectItem> {
    let (mut p, prow) = load_project(tx, project_id, true).await?;
    let (mut rec, row) = load_item(tx, project_id, item_id, true).await?;
    let now_archived = rec.item.as_ref().is_some_and(|i| i.archived);
    let fields = load_fields(tx, project_id).await?;
    if now_archived == archive {
        return Ok(render_item(&rec, &fields, &[]));
    }
    if let Some(it) = rec.item.as_mut() {
        it.archived = archive;
        it.archived_at = archive.then(|| ts(tx.now_ms()));
    }
    let v = save_item(tx, project_id, &mut rec, row.version, true).await?;
    if let Some(i) = rec.item.as_mut() {
        i.version = v;
    }
    if archive {
        p.item_count = p.item_count.saturating_sub(1);
    } else {
        p.item_count += 1;
    }
    save_project(tx, &mut p, prow.version).await?;
    log_item(
        tx,
        project_id,
        item_id,
        if archive {
            log_kind::REMOVED
        } else {
            log_kind::RESTORED
        },
        "",
        "",
        0.0,
    )
    .await?;
    let item = render_item(&rec, &fields, &[]);
    let (act, name) = if archive {
        ("archived", "project_item.archive")
    } else {
        ("restored", "project_item.unarchive")
    };
    item_event(tx, &p, act, name, &item).await?;
    Ok(item)
}

impl Work {
    /// Adds an issue or change request to a project.
    pub async fn add_item(
        &self,
        cx: &Cx,
        actor: &Actor,
        project_id: &str,
        content: ItemContent,
    ) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let item = match content {
            ItemContent::Issue(t) => {
                let (issue, _) = load_target(&mut tx, &t, false).await?;
                let repo = RepoInfo {
                    id: issue.repo_id.clone(),
                    org_id: issue.org_id.clone(),
                    full_name: String::new(),
                };
                add_issue_in_tx(&mut tx, &repo, project_id, &issue, &actor.principal).await?
            }
            ItemContent::Change { change, author_id } => {
                add_change_in_tx(&mut tx, project_id, &change, &author_id, &actor.principal).await?
            }
        };
        tx.commit().await?;
        Ok(item)
    }

    /// Creates a draft item.
    pub async fn create_draft_item(
        &self,
        cx: &Cx,
        actor: &Actor,
        project_id: &str,
        title: &str,
        body: &str,
        assignees: &[Principal],
    ) -> Result<ProjectItem> {
        let title = valid_name("title", title, limits::TITLE)?;
        if body.len() > limits::BODY {
            return Err(ForgeError::too_large(
                "body_bytes",
                limits::BODY as u64,
                body.len() as u64,
            ));
        }
        if assignees.len() > limits::ASSIGNEES {
            return Err(exhausted("assignees", limits::ASSIGNEES, assignees.len()));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, prow) = load_project(&mut tx, project_id, true).await?;
        if p.closed {
            return Err(ForgeError::invalid_state("the project is closed"));
        }
        if p.item_count as usize >= limits::PROJECT_ITEMS {
            return Err(exhausted(
                "project_items",
                limits::PROJECT_ITEMS,
                p.item_count as usize + 1,
            ));
        }
        let pos = end_position(&mut tx, project_id).await?;
        let id = tx.new_id()?;
        let mut rec = new_item(
            id.clone(),
            project_id,
            ProjectItemKind::Draft,
            &title,
            "draft",
            pos,
            &actor.principal,
            tx.now_ms(),
        );
        if let Some(it) = rec.item.as_mut() {
            it.body = body.to_string();
            let mut seen = std::collections::BTreeSet::new();
            it.assignees = assignees
                .iter()
                .filter(|a| seen.insert(a.id.clone()))
                .cloned()
                .collect();
        }
        save_item(&mut tx, project_id, &mut rec, 0, false).await?;
        p.item_count += 1;
        save_project(&mut tx, &mut p, prow.version).await?;
        log_item(&mut tx, project_id, &id, log_kind::ADDED, "", "", 0.0).await?;
        super::automation::on_item_added(&mut tx, &p, &id).await?;
        let (rec, _) = load_item(&mut tx, project_id, &id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let item = render_item(&rec, &fields, &[]);
        item_event(&mut tx, &p, "added", "project_item.create_draft", &item).await?;
        tx.commit().await?;
        Ok(item)
    }

    /// Edits a draft item.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_draft_item(
        &self,
        cx: &Cx,
        project_id: &str,
        item_id: &str,
        title: Option<String>,
        body: Option<String>,
        assignees: Option<&[Principal]>,
        expected_version: u64,
    ) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (mut rec, row) = load_item(&mut tx, project_id, item_id, true).await?;
        check_version(expected_version, row.version)?;
        if rec.item.as_ref().map(|i| i.kind) != Some(ProjectItemKind::Draft as i32) {
            return Err(ForgeError::invalid_state(
                "only draft items can be edited here",
            ));
        }
        {
            let it = rec
                .item
                .as_mut()
                .ok_or_else(|| ForgeError::internal("item without body"))?;
            if let Some(t) = title {
                it.title = valid_name("title", &t, limits::TITLE)?;
            }
            if let Some(b) = body {
                if b.len() > limits::BODY {
                    return Err(ForgeError::too_large(
                        "body_bytes",
                        limits::BODY as u64,
                        b.len() as u64,
                    ));
                }
                it.body = b;
            }
            if let Some(a) = assignees {
                if a.len() > limits::ASSIGNEES {
                    return Err(exhausted("assignees", limits::ASSIGNEES, a.len()));
                }
                let mut seen = std::collections::BTreeSet::new();
                it.assignees = a
                    .iter()
                    .filter(|x| seen.insert(x.id.clone()))
                    .cloned()
                    .collect();
            }
        }
        let v = save_item(&mut tx, project_id, &mut rec, row.version, true).await?;
        if let Some(i) = rec.item.as_mut() {
            i.version = v;
        }
        let fields = load_fields(&mut tx, project_id).await?;
        let item = render_item(&rec, &fields, &[]);
        item_event(&mut tx, &p, "edited", "project_item.update_draft", &item).await?;
        tx.commit().await?;
        Ok(item)
    }

    /// Turns a draft into an issue in `repo`, keeping its field values.
    pub async fn convert_draft_item(
        &self,
        cx: &Cx,
        actor: &Actor,
        repo: &RepoInfo,
        project_id: &str,
        item_id: &str,
    ) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (draft, _) = load_item(&mut tx, project_id, item_id, true).await?;
        let di = draft.item.clone().unwrap_or_default();
        if di.kind != ProjectItemKind::Draft as i32 {
            return Err(ForgeError::invalid_state("the item is not a draft"));
        }
        let issue = super::issues::create_issue_in_tx(
            &mut tx,
            repo,
            actor,
            NewIssue {
                title: di.title.clone(),
                body: di.body.clone(),
                assignees: di.assignees.clone(),
                effects: TextEffects::default(),
                ..Default::default()
            },
        )
        .await?;
        let (irec, _) = load_issue_id(&mut tx, &issue.id, false)
            .await?
            .ok_or_else(|| ForgeError::internal("issue vanished"))?;
        let snap = snapshot_issue(&mut tx, &irec).await?;
        let (mut rec, row) = load_item(&mut tx, project_id, item_id, true).await?;
        apply_snapshot(&mut rec, &snap);
        rec.content_key = format!("issue:{}", irec.id);
        if let Some(it) = rec.item.as_mut() {
            it.kind = ProjectItemKind::Issue as i32;
            it.body.clear();
            it.content = Some(subject(SubjectKind::Issue, &irec.repo_id, &irec.id));
        }
        let v = save_item(&mut tx, project_id, &mut rec, row.version, true).await?;
        if let Some(i) = rec.item.as_mut() {
            i.version = v;
        }
        tx.insert(
            &ITEM_INDEX,
            &Row::new(rec.content_key.clone(), item_id).k1(project_id),
        )
        .await?;
        let pid = project_id.to_string();
        add_timeline(
            &mut tx,
            &irec.id,
            Tk::AddedToProject,
            &actor.principal,
            |e| e.project_id = pid,
        )
        .await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let item = render_item(&rec, &fields, &[]);
        item_event(
            &mut tx,
            &p,
            "converted",
            "project_item.convert_draft",
            &item,
        )
        .await?;
        tx.commit().await?;
        Ok(item)
    }

    /// One item.
    pub async fn get_item(&self, cx: &Cx, project_id: &str, item_id: &str) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        let (rec, _) = load_item(&mut tx, project_id, item_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        Ok(render_item(&rec, &fields, &[]))
    }

    /// Items by a view, a filter and manual order. The page token is a position for the
    /// default order and an offset when a view sorts.
    pub async fn list_items(
        &self,
        cx: &Cx,
        project_id: &str,
        q: &ItemQuery,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<ProjectItem>, u64, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let view: Option<ProjectView> = if q.view_id.is_empty() {
            None
        } else {
            Some(super::views::find_view(&mut tx, project_id, &q.view_id).await?)
        };
        let view_filter = match &view {
            Some(v) if !v.filter.is_empty() => Some(Compiled::new(&v.filter, &fields)?),
            _ => None,
        };
        let query_filter = if q.query.trim().is_empty() {
            None
        } else {
            Some(Compiled::new(&q.query, &fields)?)
        };
        let sorted = view.as_ref().is_some_and(|v| !v.sort_by.is_empty());
        let narrowed = sorted || view_filter.is_some() || query_filter.is_some();
        let mut items: Vec<ItemRec> =
            scan_all_msgs::<ItemRec>(&mut tx, &PROJECT_ITEMS, Scan::scope(project_id, 1).by_n1())
                .await?
                .into_iter()
                .map(|(mut r, row)| {
                    if let Some(i) = r.item.as_mut() {
                        i.version = row.version;
                    }
                    r
                })
                .filter(|r| {
                    let archived = r.item.as_ref().is_some_and(|i| i.archived);
                    if q.archived_only {
                        return archived;
                    }
                    q.include_archived || !archived
                })
                .filter(|r| !narrowed || item_repo(r).is_none_or(|id| !q.hidden_repos.contains(id)))
                .filter(|r| view_filter.as_ref().is_none_or(|f| f.matches(r, &q.ctx)))
                .filter(|r| query_filter.as_ref().is_none_or(|f| f.matches(r, &q.ctx)))
                .collect();
        if let Some(v) = &view {
            sort_items(&mut items, &fields, &v.sort_by);
        }
        let total = items.len() as u64;
        let rest: Vec<ItemRec> = if sorted {
            let off: usize = after
                .strip_prefix('#')
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            items.into_iter().skip(off).collect()
        } else {
            items
                .into_iter()
                .filter(|r| {
                    after.is_empty() || r.item.as_ref().is_some_and(|i| i.position.as_str() > after)
                })
                .collect()
        };
        let next = if rest.len() > limit {
            if sorted {
                let off: usize = after
                    .strip_prefix('#')
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                format!("#{}", off + limit)
            } else {
                rest[limit - 1]
                    .item
                    .as_ref()
                    .map(|i| i.position.clone())
                    .unwrap_or_default()
            }
        } else {
            String::new()
        };
        let out = rest
            .iter()
            .take(limit)
            .map(|r| render_item(r, &fields, &q.field_ids))
            .collect();
        Ok((out, total, next))
    }

    /// The ids of the repos a project's items come from (the caller decides which it may read).
    pub async fn item_repo_ids(&self, cx: &Cx, project_id: &str) -> Result<BTreeSet<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        Ok(
            scan_all_msgs::<ItemRec>(&mut tx, &PROJECT_ITEMS, Scan::scope(project_id, 1).by_n1())
                .await?
                .iter()
                .filter_map(|(r, _)| item_repo(r).map(str::to_string))
                .collect(),
        )
    }

    /// Moves an item after another (empty: to the top).
    pub async fn move_item(
        &self,
        cx: &Cx,
        project_id: &str,
        item_id: &str,
        after_item_id: &str,
    ) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (mut rec, row) = load_item(&mut tx, project_id, item_id, true).await?;
        let list: Vec<(String, i64)> =
            scan_all(&mut tx, &PROJECT_ITEMS, Scan::scope(project_id, 1).by_n1())
                .await?
                .into_iter()
                .filter(|r| r.id != item_id)
                .map(|r| (r.id, r.n1.unwrap_or(BASE)))
                .collect();
        let pos = if after_item_id.is_empty() {
            list.first().map(|(_, p)| p - GAP).unwrap_or(BASE)
        } else {
            let i = list
                .iter()
                .position(|(id, _)| id == after_item_id)
                .ok_or_else(|| ForgeError::not_found(format!("project item {after_item_id}")))?;
            let lo = list[i].1;
            match list.get(i + 1).map(|(_, p)| *p) {
                None => lo + GAP,
                Some(hi) if hi - lo >= 2 => lo + (hi - lo) / 2,
                Some(_) => {
                    // No room: spread the whole list, then place after the anchor.
                    for (k, (id, _)) in list.iter().enumerate() {
                        let (mut other, orow) = load_item(&mut tx, project_id, id, true).await?;
                        if let Some(it) = other.item.as_mut() {
                            it.position = pos_str(BASE + (k as i64) * GAP * 2);
                        }
                        save_item(&mut tx, project_id, &mut other, orow.version, false).await?;
                    }
                    BASE + (i as i64) * GAP * 2 + GAP
                }
            }
        };
        if let Some(it) = rec.item.as_mut() {
            it.position = pos_str(pos);
        }
        let v = save_item(&mut tx, project_id, &mut rec, row.version, false).await?;
        if let Some(i) = rec.item.as_mut() {
            i.version = v;
        }
        let fields = load_fields(&mut tx, project_id).await?;
        let item = render_item(&rec, &fields, &[]);
        item_event(&mut tx, &p, "reordered", "project_item.move", &item).await?;
        tx.commit().await?;
        Ok(item)
    }

    async fn set_archived(
        &self,
        cx: &Cx,
        project_id: &str,
        item_id: &str,
        archive: bool,
    ) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let item = set_archived_in_tx(&mut tx, project_id, item_id, archive).await?;
        tx.commit().await?;
        Ok(item)
    }

    /// Archives an item.
    pub async fn archive_item(
        &self,
        cx: &Cx,
        project_id: &str,
        item_id: &str,
    ) -> Result<ProjectItem> {
        self.set_archived(cx, project_id, item_id, true).await
    }

    /// Restores an archived item.
    pub async fn unarchive_item(
        &self,
        cx: &Cx,
        project_id: &str,
        item_id: &str,
    ) -> Result<ProjectItem> {
        self.set_archived(cx, project_id, item_id, false).await
    }

    /// Removes an item from the project (the issue or change request is untouched).
    pub async fn delete_item(
        &self,
        cx: &Cx,
        actor: &Actor,
        project_id: &str,
        item_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, prow) = load_project(&mut tx, project_id, true).await?;
        let (rec, _) = load_item(&mut tx, project_id, item_id, true).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let item = render_item(&rec, &fields, &[]);
        let archived = rec.item.as_ref().is_some_and(|i| i.archived);
        drop_item_rows(&mut tx, &p, &rec, item_id, &actor.principal, true).await?;
        if open_count_delta(&rec) == 1 && !archived {
            p.item_count = p.item_count.saturating_sub(1);
        }
        save_project(&mut tx, &mut p, prow.version).await?;
        log_item(&mut tx, project_id, item_id, log_kind::REMOVED, "", "", 0.0).await?;
        item_event(&mut tx, &p, "deleted", "project_item.delete", &item).await?;
        tx.commit().await
    }

    /// Sets one field value on an item.
    pub async fn set_item_value(
        &self,
        cx: &Cx,
        actor: &Actor,
        project_id: &str,
        item_id: &str,
        value: &ProjectFieldValue,
        expected_version: u64,
    ) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let key = if value.field_id.is_empty() {
            &value.field_name
        } else {
            &value.field_id
        };
        if key.is_empty() {
            return Err(ForgeError::missing("field_id"));
        }
        let field = fields
            .iter()
            .find(|f| f.id == *key || f.name.eq_ignore_ascii_case(key))
            .ok_or_else(|| {
                ForgeError::invalid("unknown_field", format!("no project field {key:?}"))
            })?
            .clone();
        let (mut rec, row) = load_item(&mut tx, project_id, item_id, true).await?;
        check_version(expected_version, row.version)?;
        let nv = super::views::validate_item_value(&field, value)?;
        let changed =
            set_value_in_tx(&mut tx, &p, &mut rec, &field, Some(nv), &actor.principal).await?;
        let mut version = row.version;
        if changed {
            version = save_item(&mut tx, project_id, &mut rec, row.version, true).await?;
            if let Some(i) = rec.item.as_mut() {
                i.version = version;
            }
            super::automation::after_field_set(&mut tx, &p, item_id).await?;
        }
        let (rec, _) = load_item(&mut tx, project_id, item_id, false).await?;
        let item = render_item(&rec, &fields, &[]);
        if changed {
            item_event(&mut tx, &p, "edited", "project_item.set_value", &item).await?;
        }
        let _ = version;
        tx.commit().await?;
        Ok(item)
    }

    /// Clears one field value.
    pub async fn clear_item_value(
        &self,
        cx: &Cx,
        actor: &Actor,
        project_id: &str,
        item_id: &str,
        field: &str,
    ) -> Result<ProjectItem> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let f = fields
            .iter()
            .find(|f| f.id == field || f.name.eq_ignore_ascii_case(field))
            .ok_or_else(|| {
                ForgeError::invalid("unknown_field", format!("no project field {field:?}"))
            })?
            .clone();
        let (mut rec, row) = load_item(&mut tx, project_id, item_id, true).await?;
        let changed = set_value_in_tx(&mut tx, &p, &mut rec, &f, None, &actor.principal).await?;
        if changed {
            let v = save_item(&mut tx, project_id, &mut rec, row.version, true).await?;
            if let Some(i) = rec.item.as_mut() {
                i.version = v;
            }
        }
        let item = render_item(&rec, &fields, &[]);
        if changed {
            item_event(&mut tx, &p, "edited", "project_item.clear_value", &item).await?;
        }
        tx.commit().await?;
        Ok(item)
    }
}

/// Sets or clears a value on an in-memory item and records the activity; returns whether it
/// changed. The caller saves the item.
pub(crate) async fn set_value_in_tx(
    tx: &mut Tx,
    p: &Project,
    rec: &mut ItemRec,
    f: &ProjectField,
    nv: Option<ProjectFieldValue>,
    actor: &Principal,
) -> Result<bool> {
    let item_id = rec.item.as_ref().map(|i| i.id.clone()).unwrap_or_default();
    let prev = rec
        .item
        .as_ref()
        .and_then(|i| i.field_values.iter().find(|v| v.field_id == f.id).cloned());
    let prev_display = prev.as_ref().map(|v| display_of(f, v)).unwrap_or_default();
    if prev == nv {
        return Ok(false);
    }
    let it = rec
        .item
        .as_mut()
        .ok_or_else(|| ForgeError::internal("item without body"))?;
    it.field_values.retain(|v| v.field_id != f.id);
    let (display, number) = match &nv {
        Some(v) => (
            display_of(f, v),
            match &v.value {
                Some(Value::Number(n)) => *n,
                _ => 0.0,
            },
        ),
        None => (String::new(), 0.0),
    };
    if let Some(v) = nv {
        it.field_values.push(v);
    }
    let kind = if display.is_empty() && number == 0.0 {
        log_kind::CLEARED
    } else {
        log_kind::SET
    };
    log_item(tx, &p.id, &item_id, kind, &f.id, &display, number).await?;
    // The issue's own timeline notes status moves.
    if f.name.eq_ignore_ascii_case("status")
        && let Some(issue_id) = rec.content_key.strip_prefix("issue:")
        && load_issue_id(tx, issue_id, false).await?.is_some()
    {
        let pid = p.id.clone();
        add_timeline(tx, issue_id, Tk::ProjectStatusChanged, actor, |e| {
            e.project_id = pid;
            e.field_name = f.name.clone();
            e.previous_value = prev_display;
            e.current_value = display;
        })
        .await?;
    }
    Ok(true)
}
