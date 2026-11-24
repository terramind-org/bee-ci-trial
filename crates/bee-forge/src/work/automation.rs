//! Project automations: event-driven workflows (set a field when an item is added, closed,
//! reopened or landed; auto-add, auto-archive, auto-close, auto-add sub-issues).
//!
//! Issues and projects share the `work` surface, so a workflow runs in the same transaction
//! as the change that triggers it: an issue closed through the API and its item moving to
//! `Done` commit together. Change request events arrive through the event listener.

use bee_proto::v1::{
    IssueStateReason, Project, ProjectField, ProjectFieldKind, ProjectFieldValue, ProjectWorkflow,
    ProjectWorkflowKind as K, project_field_value::Value,
};

use super::issues::load_issue_id;
use super::items::{
    add_change_in_tx, add_issue_in_tx, load_item, render_item, save_item, set_archived_in_tx,
    set_value_in_tx, snapshot_issue,
};
use super::model::{IssueRec, ItemRec};
use super::pfilter::{Compiled, EvalCtx};
use super::projects::{automation_principal, load_fields, load_project, org_of, pchange};
use super::relations::ChangeLink;
use super::*;
use crate::ids::ts;

const DAY_MS: u64 = 86_400_000;

fn kind_of(w: &ProjectWorkflow) -> K {
    K::try_from(w.kind).unwrap_or(K::Unspecified)
}

async fn workflows(tx: &mut Tx, project_id: &str) -> Result<Vec<ProjectWorkflow>> {
    let mut v: Vec<ProjectWorkflow> =
        scan_all_msgs::<ProjectWorkflow>(tx, &WORKFLOWS, Scan::scope(project_id, 1))
            .await?
            .into_iter()
            .map(|(mut w, r)| {
                w.version = r.version;
                w
            })
            .collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(v)
}

/// Sets the workflow's field to its option on an item.
async fn apply_set(tx: &mut Tx, p: &Project, item_id: &str, w: &ProjectWorkflow) -> Result<()> {
    if w.field_id.is_empty() || w.option_id.is_empty() {
        return Ok(());
    }
    let fields = load_fields(tx, &p.id).await?;
    let Some(f) = fields.iter().find(|f| f.id == w.field_id) else {
        return Ok(());
    };
    if !f.options.iter().any(|o| o.id == w.option_id) {
        return Ok(());
    }
    let (mut rec, row) = load_item(tx, &p.id, item_id, true).await?;
    let nv = ProjectFieldValue {
        field_id: f.id.clone(),
        field_name: f.name.clone(),
        kind: f.kind,
        value: Some(Value::OptionId(w.option_id.clone())),
        display_value: String::new(),
    };
    if set_value_in_tx(tx, p, &mut rec, f, Some(nv), &automation_principal()).await? {
        save_item(tx, &p.id, &mut rec, row.version, true).await?;
        record_automation(tx, p, w, &rec, &fields).await?;
        Box::pin(after_field_set(tx, p, item_id)).await?;
    }
    Ok(())
}

async fn record_automation(
    tx: &mut Tx,
    p: &Project,
    w: &ProjectWorkflow,
    rec: &ItemRec,
    fields: &[ProjectField],
) -> Result<()> {
    let org = org_of(p);
    let item = render_item(rec, fields, &[]);
    record(
        tx,
        pchange(
            "project_item.changed",
            "project_item.automation",
            &org,
            p,
            serde_json::json!({
                "action": "edited",
                "project_id": p.id,
                "workflow": w.name,
                "item": serde_json::from_str::<serde_json::Value>(&pjson!(&item)).unwrap_or_default(),
            })
            .to_string(),
            serde_json::json!({"project": p.number, "item": item.id, "workflow": w.name}).to_string(),
        ),
    )
    .await
}

/// An item joined a project: `ItemAdded` workflows and sub-issue pull-in.
pub(crate) async fn on_item_added(tx: &mut Tx, p: &Project, item_id: &str) -> Result<()> {
    for w in workflows(tx, &p.id).await? {
        if !w.enabled {
            continue;
        }
        match kind_of(&w) {
            K::ItemAdded => apply_set(tx, p, item_id, &w).await?,
            K::AutoAddSubIssues => {
                let (rec, _) = load_item(tx, &p.id, item_id, false).await?;
                let Some(issue_id) = rec.content_key.strip_prefix("issue:") else {
                    continue;
                };
                let kids = tx
                    .scan(
                        &SUBS,
                        &Scan::scope(issue_id, limits::SUB_ISSUES + 1).by_n1(),
                    )
                    .await?;
                for k in kids {
                    let key = format!("issue:{}", k.id);
                    let present = !tx
                        .scan(
                            &PROJECT_ITEMS,
                            &Scan::scope(p.id.clone(), 1).key(Key::K1(key)),
                        )
                        .await?
                        .is_empty();
                    if present {
                        continue;
                    }
                    if let Some((child, _)) = load_issue_id(tx, &k.id, false).await? {
                        let repo = RepoInfo {
                            id: child.repo_id.clone(),
                            org_id: child.org_id.clone(),
                            full_name: String::new(),
                        };
                        let (fresh, _) = load_project(tx, &p.id, true).await?;
                        if !fresh.closed {
                            Box::pin(add_issue_in_tx(
                                tx,
                                &repo,
                                &p.id,
                                &child,
                                &automation_principal(),
                            ))
                            .await?;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// A field changed on an item: `AutoCloseIssue` closes the issue when its watched status is
/// reached.
pub(crate) async fn after_field_set(tx: &mut Tx, p: &Project, item_id: &str) -> Result<()> {
    let ws = workflows(tx, &p.id).await?;
    if !ws
        .iter()
        .any(|w| w.enabled && kind_of(w) == K::AutoCloseIssue)
    {
        return Ok(());
    }
    let (rec, _) = load_item(tx, &p.id, item_id, false).await?;
    let Some(issue_id) = rec.content_key.strip_prefix("issue:").map(str::to_string) else {
        return Ok(());
    };
    let item = rec.item.clone().unwrap_or_default();
    for w in ws
        .iter()
        .filter(|w| w.enabled && kind_of(w) == K::AutoCloseIssue)
    {
        let hit = item.field_values.iter().any(|v| {
            v.field_id == w.field_id && v.value == Some(Value::OptionId(w.option_id.clone()))
        });
        if hit {
            super::issues::close_issue_in_tx(
                tx,
                &issue_id,
                &automation_principal(),
                IssueStateReason::Completed,
                "",
            )
            .await?;
            break;
        }
    }
    Ok(())
}

/// The items that show `content_key`, as `(project, item id)`.
async fn items_of(tx: &mut Tx, content_key: &str) -> Result<Vec<(String, String)>> {
    Ok(scan_all(tx, &ITEM_INDEX, Scan::scope(content_key, 1))
        .await?
        .into_iter()
        .filter_map(|r| r.k1.map(|p| (p, r.id)))
        .collect())
}

async fn run_kind(tx: &mut Tx, content_key: &str, kind: K) -> Result<()> {
    for (project_id, item_id) in items_of(tx, content_key).await? {
        let Ok((p, _)) = load_project(tx, &project_id, false).await else {
            continue;
        };
        for w in workflows(tx, &project_id).await? {
            if w.enabled && kind_of(&w) == kind {
                apply_set(tx, &p, &item_id, &w).await?;
            }
        }
    }
    Ok(())
}

/// An issue closed or reopened.
pub(crate) async fn on_issue_state(
    tx: &mut Tx,
    rec: &IssueRec,
    closed: bool,
    _change: Option<&str>,
) -> Result<()> {
    run_kind(
        tx,
        &format!("issue:{}", rec.id),
        if closed {
            K::ItemClosed
        } else {
            K::ItemReopened
        },
    )
    .await
}

/// What happened to a change request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeTrigger {
    /// Landed.
    Landed,
    /// Closed without landing.
    Closed,
    /// Reopened.
    Reopened,
    /// A review requested changes.
    ChangesRequested,
    /// A review approved.
    Approved,
}

/// A change request event reached the work module (through the listener).
pub(crate) async fn on_change(tx: &mut Tx, change_id: &str, trigger: ChangeTrigger) -> Result<()> {
    let key = format!("cr:{change_id}");
    match trigger {
        ChangeTrigger::Landed => run_kind(tx, &key, K::ChangeLanded).await,
        ChangeTrigger::Closed => run_kind(tx, &key, K::ItemClosed).await,
        ChangeTrigger::Reopened => run_kind(tx, &key, K::ItemReopened).await,
        ChangeTrigger::ChangesRequested => run_kind(tx, &key, K::ChangesRequested).await,
        ChangeTrigger::Approved => run_kind(tx, &key, K::ChangeApproved).await,
    }
}

/// Ids of the projects linked to `repo` whose enabled auto-add workflow matches `probe` and
/// that do not hold `content_key` yet.
async fn auto_add_matches(
    tx: &mut Tx,
    repo: &RepoInfo,
    probe: &ItemRec,
    content_key: &str,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let links = tx
        .scan(&PROJECT_REPOS, &Scan::scope(repo.id.clone(), 500))
        .await?;
    for l in links {
        let Ok((p, _)) = load_project(tx, &l.id, false).await else {
            continue;
        };
        if p.closed {
            continue;
        }
        let fields = load_fields(tx, &p.id).await?;
        let ctx = EvalCtx {
            repos: [(repo.full_name.to_lowercase(), repo.id.clone())]
                .into_iter()
                .collect(),
            now_ms: tx.now_ms(),
            ..Default::default()
        };
        let mut hit = false;
        for w in workflows(tx, &p.id)
            .await?
            .iter()
            .filter(|w| w.enabled && kind_of(w) == K::AutoAdd)
        {
            let source_ok = match &w.source_repo {
                Some(r) => {
                    matches!(&r.repo, Some(bee_proto::v1::repo_ref::Repo::Id(id)) if *id == repo.id)
                }
                None => true,
            };
            if !source_ok {
                continue;
            }
            let ok = w.filter.trim().is_empty()
                || Compiled::new(&w.filter, &fields)
                    .map(|c| c.matches(probe, &ctx))
                    .unwrap_or(false);
            if ok {
                hit = true;
                break;
            }
        }
        if hit {
            let present = !tx
                .scan(
                    &PROJECT_ITEMS,
                    &Scan::scope(p.id.clone(), 1).key(Key::K1(content_key.to_string())),
                )
                .await?
                .is_empty();
            if !present {
                out.push(p.id.clone());
            }
        }
    }
    Ok(out)
}

/// A new issue appeared in `repo`: `AutoAdd` workflows of projects linked to it add it when
/// it matches their filter.
pub(crate) async fn on_issue_created(tx: &mut Tx, repo: &RepoInfo, issue: &IssueRec) -> Result<()> {
    let snap = snapshot_issue(tx, issue).await?;
    let probe = ItemRec {
        item: Some(bee_proto::v1::ProjectItem {
            kind: bee_proto::v1::ProjectItemKind::Issue as i32,
            title: snap.title.clone(),
            content_state: snap.state.into(),
            assignees: snap.assignees.clone(),
            ..Default::default()
        }),
        labels: snap.labels.clone(),
        milestone: snap.milestone.clone(),
        issue_type: snap.issue_type.clone(),
        repo_id: snap.repo_id.clone(),
        author_id: snap.author_id.clone(),
        parent: snap.parent.clone(),
        ..Default::default()
    };
    for pid in auto_add_matches(tx, repo, &probe, &format!("issue:{}", issue.id)).await? {
        add_issue_in_tx(tx, repo, &pid, issue, &automation_principal()).await?;
    }
    Ok(())
}

/// A change request was opened in `repo`: the same for change requests.
pub(crate) async fn on_change_created(
    tx: &mut Tx,
    repo: &RepoInfo,
    change: &ChangeLink,
    author_id: &str,
) -> Result<()> {
    let probe = ItemRec {
        item: Some(bee_proto::v1::ProjectItem {
            kind: bee_proto::v1::ProjectItemKind::ChangeRequest as i32,
            title: change.title.clone(),
            content_state: change.state.clone(),
            ..Default::default()
        }),
        repo_id: change.repo_id.clone(),
        author_id: author_id.to_string(),
        ..Default::default()
    };
    for pid in auto_add_matches(tx, repo, &probe, &format!("cr:{}", change.id)).await? {
        add_change_in_tx(tx, &pid, change, author_id, &automation_principal()).await?;
    }
    Ok(())
}

fn workflow_audit(p: &Project, w: &ProjectWorkflow) -> String {
    serde_json::json!({"project": p.number, "workflow": w.name, "kind": w.kind, "enabled": w.enabled}).to_string()
}

fn workflow_change<'a>(
    action: &'a str,
    org: &'a str,
    p: &'a Project,
    w: &ProjectWorkflow,
) -> Change<'a> {
    pchange(
        "project_config.changed",
        action,
        org,
        p,
        pjson!(w),
        workflow_audit(p, w),
    )
}

/// A workflow to create.
#[derive(Clone, Debug, Default)]
pub struct WorkflowSpec {
    /// What triggers it.
    pub kind: K,
    /// Name (empty = from the kind).
    pub name: String,
    /// Filter for the auto-add and auto-archive kinds.
    pub filter: String,
    /// Repo for auto-add; it must be linked to the project.
    pub source_repo: Option<RepoInfo>,
    /// Field to set, or to watch for auto-close.
    pub field_id: String,
    /// Option to set, or to watch.
    pub option_id: String,
    /// Days of inactivity before auto-archive.
    pub archive_after_days: u32,
    /// Start enabled.
    pub enabled: bool,
}

/// A partial workflow edit.
#[derive(Clone, Debug, Default)]
pub struct WorkflowPatch {
    /// New name.
    pub name: Option<String>,
    /// New filter.
    pub filter: Option<String>,
    /// New source repo.
    pub source_repo: Option<RepoInfo>,
    /// New field.
    pub field_id: Option<String>,
    /// New option.
    pub option_id: Option<String>,
    /// New inactivity period.
    pub archive_after_days: Option<u32>,
}

fn default_name(k: K) -> &'static str {
    match k {
        K::ItemAdded => "Item added to project",
        K::ItemReopened => "Item reopened",
        K::ItemClosed => "Item closed",
        K::ChangeLanded => "Change landed",
        K::ChangesRequested => "Changes requested",
        K::ChangeApproved => "Change approved",
        K::AutoArchive => "Auto-archive items",
        K::AutoAdd => "Auto-add to project",
        K::AutoCloseIssue => "Auto-close issue",
        K::AutoAddSubIssues => "Auto-add sub-issues",
        K::Unspecified => "Workflow",
    }
}

fn validate_workflow(w: &ProjectWorkflow, fields: &[ProjectField], p: &Project) -> Result<()> {
    let k = kind_of(w);
    match k {
        K::Unspecified => return Err(ForgeError::missing("kind")),
        K::ItemAdded
        | K::ItemReopened
        | K::ItemClosed
        | K::ChangeLanded
        | K::ChangesRequested
        | K::ChangeApproved
        | K::AutoCloseIssue => {
            let f = fields.iter().find(|f| f.id == w.field_id).ok_or_else(|| {
                ForgeError::invalid("field_required", "this workflow needs `field_id`")
            })?;
            if f.kind != ProjectFieldKind::SingleSelect as i32 {
                return Err(ForgeError::invalid(
                    "invalid_field",
                    "workflows set single-select fields",
                ));
            }
            if !f.options.iter().any(|o| o.id == w.option_id) {
                return Err(ForgeError::invalid(
                    "unknown_option",
                    "`option_id` is not an option of the field",
                ));
            }
        }
        K::AutoArchive => {
            if w.archive_after_days == 0 {
                return Err(ForgeError::invalid(
                    "days_required",
                    "auto-archive needs `archive_after_days` of at least 1",
                ));
            }
            Compiled::new(&w.filter, fields)?;
        }
        K::AutoAdd => {
            Compiled::new(&w.filter, fields)?;
            let linked = match &w.source_repo {
                Some(r) => match &r.repo {
                    Some(bee_proto::v1::repo_ref::Repo::Id(id)) => p.linked_repo_ids.contains(id),
                    _ => false,
                },
                None => return Err(ForgeError::missing("source_repo")),
            };
            if !linked {
                return Err(ForgeError::invalid(
                    "repo_not_linked",
                    "`source_repo` must be linked to the project first",
                ));
            }
        }
        K::AutoAddSubIssues => {}
    }
    Ok(())
}

impl Work {
    /// A project's workflows.
    pub async fn list_workflows(&self, cx: &Cx, project_id: &str) -> Result<Vec<ProjectWorkflow>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        workflows(&mut tx, project_id).await
    }

    /// Creates a workflow.
    pub async fn create_workflow(
        &self,
        cx: &Cx,
        project_id: &str,
        spec: WorkflowSpec,
    ) -> Result<ProjectWorkflow> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let count = tx.count(&WORKFLOWS, project_id, None).await? as usize;
        if count >= limits::WORKFLOWS {
            return Err(exhausted("project_workflows", limits::WORKFLOWS, count + 1));
        }
        let name = if spec.name.trim().is_empty() {
            default_name(spec.kind).to_string()
        } else {
            valid_name("name", &spec.name, 100)?
        };
        let w = ProjectWorkflow {
            id: tx.new_id()?,
            project_id: project_id.to_string(),
            kind: spec.kind as i32,
            name,
            enabled: spec.enabled,
            filter: spec.filter.trim().to_string(),
            source_repo: spec.source_repo.as_ref().map(|r| repo_ref_id(&r.id)),
            field_id: spec.field_id,
            option_id: spec.option_id,
            archive_after_days: spec.archive_after_days,
            is_default: false,
            updated_at: Some(ts(tx.now_ms())),
            version: 1,
        };
        validate_workflow(&w, &fields, &p)?;
        tx.insert(&WORKFLOWS, &Row::new(project_id, w.id.clone()).msg(&w))
            .await?;
        let org = org_of(&p);
        record(
            &mut tx,
            workflow_change("project_config.create_workflow", &org, &p, &w),
        )
        .await?;
        tx.commit().await?;
        Ok(w)
    }

    /// Edits a workflow.
    pub async fn update_workflow(
        &self,
        cx: &Cx,
        project_id: &str,
        workflow_id: &str,
        patch: WorkflowPatch,
        expected_version: u64,
    ) -> Result<ProjectWorkflow> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let row = tx
            .get_for_update(&WORKFLOWS, project_id, workflow_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("workflow {workflow_id}")))?;
        check_version(expected_version, row.version)?;
        let mut w: ProjectWorkflow = row.decode()?;
        if let Some(n) = patch.name {
            w.name = valid_name("name", &n, 100)?;
        }
        if let Some(f) = patch.filter {
            w.filter = f.trim().to_string();
        }
        if let Some(r) = patch.source_repo {
            w.source_repo = Some(repo_ref_id(&r.id));
        }
        if let Some(f) = patch.field_id {
            w.field_id = f;
        }
        if let Some(o) = patch.option_id {
            w.option_id = o;
        }
        if let Some(d) = patch.archive_after_days {
            w.archive_after_days = d;
        }
        validate_workflow(&w, &fields, &p)?;
        w.updated_at = Some(ts(tx.now_ms()));
        w.version = tx
            .update(
                &WORKFLOWS,
                &Row::new(project_id, workflow_id)
                    .version(row.version)
                    .msg(&w),
            )
            .await?;
        let org = org_of(&p);
        record(
            &mut tx,
            workflow_change("project_config.update_workflow", &org, &p, &w),
        )
        .await?;
        tx.commit().await?;
        Ok(w)
    }

    async fn toggle_workflow(
        &self,
        cx: &Cx,
        project_id: &str,
        workflow_id: &str,
        on: bool,
    ) -> Result<ProjectWorkflow> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let row = tx
            .get_for_update(&WORKFLOWS, project_id, workflow_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("workflow {workflow_id}")))?;
        let mut w: ProjectWorkflow = row.decode()?;
        w.version = row.version;
        if w.enabled == on {
            return Ok(w);
        }
        if on {
            validate_workflow(&w, &fields, &p)?;
        }
        w.enabled = on;
        w.updated_at = Some(ts(tx.now_ms()));
        w.version = tx
            .update(
                &WORKFLOWS,
                &Row::new(project_id, workflow_id)
                    .version(row.version)
                    .msg(&w),
            )
            .await?;
        let org = org_of(&p);
        record(
            &mut tx,
            workflow_change(
                if on {
                    "project_config.enable_workflow"
                } else {
                    "project_config.disable_workflow"
                },
                &org,
                &p,
                &w,
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(w)
    }

    /// Enables a workflow.
    pub async fn enable_workflow(
        &self,
        cx: &Cx,
        project_id: &str,
        workflow_id: &str,
    ) -> Result<ProjectWorkflow> {
        self.toggle_workflow(cx, project_id, workflow_id, true)
            .await
    }

    /// Disables a workflow.
    pub async fn disable_workflow(
        &self,
        cx: &Cx,
        project_id: &str,
        workflow_id: &str,
    ) -> Result<ProjectWorkflow> {
        self.toggle_workflow(cx, project_id, workflow_id, false)
            .await
    }

    /// Deletes a workflow; the defaults created with the project can only be disabled.
    pub async fn delete_workflow(
        &self,
        cx: &Cx,
        project_id: &str,
        workflow_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (w, _) = tx
            .get_msg::<ProjectWorkflow>(&WORKFLOWS, project_id, workflow_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("workflow {workflow_id}")))?;
        if w.is_default {
            return Err(ForgeError::invalid_state(
                "default workflows can be disabled, not deleted",
            ));
        }
        tx.delete(&WORKFLOWS, project_id, workflow_id).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            workflow_change("project_config.delete_workflow", &org, &p, &w),
        )
        .await?;
        tx.commit().await
    }

    /// Time-based automation: archives items that matched an enabled auto-archive workflow's
    /// filter and have been quiet for its period. The maintenance worker calls it.
    pub async fn sweep_projects(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let projects = scan_all_msgs::<Project>(&mut tx, &PROJECTS, Scan::default()).await?;
        let now = tx.now_ms();
        let mut archived = 0;
        for (p, _) in projects {
            if p.closed {
                continue;
            }
            let ws: Vec<ProjectWorkflow> = workflows(&mut tx, &p.id)
                .await?
                .into_iter()
                .filter(|w| w.enabled && kind_of(w) == K::AutoArchive)
                .collect();
            if ws.is_empty() {
                continue;
            }
            let fields = load_fields(&mut tx, &p.id).await?;
            let items =
                scan_all_msgs::<ItemRec>(&mut tx, &PROJECT_ITEMS, Scan::scope(p.id.clone(), 1))
                    .await?;
            for (rec, _) in items {
                let Some(it) = rec.item.clone() else { continue };
                if it.archived {
                    continue;
                }
                let idle = now
                    .saturating_sub(it.updated_at.as_ref().map(crate::ids::ts_ms).unwrap_or(now));
                for w in &ws {
                    if idle < u64::from(w.archive_after_days) * DAY_MS {
                        continue;
                    }
                    let ctx = EvalCtx {
                        now_ms: now,
                        ..Default::default()
                    };
                    let hit = Compiled::new(&w.filter, &fields)
                        .map(|c| c.matches(&rec, &ctx))
                        .unwrap_or(false);
                    if hit {
                        set_archived_in_tx(&mut tx, &p.id, &it.id, true).await?;
                        archived += 1;
                        break;
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(archived)
    }
}
