//! Projects v2: projects, status updates, repo and team links, copy and templates.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::{
    Principal, Project, ProjectField, ProjectFieldKind, ProjectFieldOption, ProjectOwnerKind,
    ProjectStatus, ProjectStatusUpdate, ProjectTeamLink, ProjectTeamRole, ProjectView,
    ProjectViewLayout, ProjectWorkflow, ProjectWorkflowKind, SubjectKind,
};

use super::model::{ItemLogRec, ItemRec};
use super::*;
use crate::ids::{ts, ts_ms};

/// Who owns a project: an org or a user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerInfo {
    /// Org or user id.
    pub id: String,
    /// Org name or user login.
    pub login: String,
    /// Org or user.
    pub kind: ProjectOwnerKind,
}

/// The built-in fields every project has, in display order: `(name, kind)`.
pub const BUILT_IN_FIELDS: &[(&str, ProjectFieldKind)] = &[
    ("Title", ProjectFieldKind::Title),
    ("Assignees", ProjectFieldKind::Assignees),
    ("Labels", ProjectFieldKind::Labels),
    ("Milestone", ProjectFieldKind::Milestone),
    ("Repository", ProjectFieldKind::Repository),
    ("Linked changes", ProjectFieldKind::LinkedChanges),
    ("Reviewers", ProjectFieldKind::Reviewers),
    ("Parent issue", ProjectFieldKind::ParentIssue),
    ("Sub-issues progress", ProjectFieldKind::SubIssuesProgress),
    ("Type", ProjectFieldKind::IssueType),
];

/// The options of the default `Status` field.
pub const STATUS_OPTIONS: &[(&str, &str)] = &[
    ("Todo", "GREEN"),
    ("In Progress", "YELLOW"),
    ("Done", "PURPLE"),
];

const TITLE_MAX: usize = 256;
const SHORT_MAX: usize = 350;

/// The org that owns the audit chain of a project (empty for user projects).
pub(crate) fn org_of(p: &Project) -> String {
    if p.owner_kind == ProjectOwnerKind::Org as i32 {
        p.owner_id.clone()
    } else {
        String::new()
    }
}

pub(crate) fn project_url(p: &Project) -> String {
    let base = if p.owner_kind == ProjectOwnerKind::User as i32 {
        "users"
    } else {
        "orgs"
    };
    format!("/{base}/{}/projects/{}", p.owner_login, p.number)
}

/// Loads a project by id.
pub(crate) async fn load_project(tx: &mut Tx, id: &str, lock: bool) -> Result<(Project, Row)> {
    let missing = || ForgeError::not_found(format!("project {id}"));
    let owner = lookup_id(tx, "project", id).await?.ok_or_else(missing)?;
    let row = if lock {
        tx.get_for_update(&PROJECTS, &owner, id).await?
    } else {
        tx.get(&PROJECTS, &owner, id).await?
    }
    .ok_or_else(missing)?;
    let mut p: Project = row.decode()?;
    p.version = row.version;
    p.url = project_url(&p);
    Ok((p, row))
}

/// Writes a project row (updated time moves); returns the new version.
pub(crate) async fn save_project(tx: &mut Tx, p: &mut Project, version: u64) -> Result<u64> {
    p.updated_at = Some(ts(tx.now_ms()));
    let row = Row::new(p.owner_id.clone(), p.id.clone())
        .k1(pad(p.number))
        .n1(p.created_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
        .version(version)
        .msg(p);
    let v = tx.update(&PROJECTS, &row).await?;
    p.version = v;
    Ok(v)
}

/// A project's fields in creation order.
pub(crate) async fn load_fields(tx: &mut Tx, project_id: &str) -> Result<Vec<ProjectField>> {
    let mut v: Vec<ProjectField> =
        scan_all_msgs::<ProjectField>(tx, &PROJECT_FIELDS, Scan::scope(project_id, 1))
            .await?
            .into_iter()
            .map(|(mut f, r)| {
                f.version = r.version;
                f
            })
            .collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(v)
}

/// The `Status` field, if the project has one.
pub(crate) fn status_field(fields: &[ProjectField]) -> Option<&ProjectField> {
    fields.iter().find(|f| {
        f.kind == ProjectFieldKind::SingleSelect as i32 && f.name.eq_ignore_ascii_case("status")
    })
}

pub(crate) fn pchange<'a>(
    kind: &'a str,
    action: &'a str,
    org: &'a str,
    p: &'a Project,
    payload: String,
    after: String,
) -> Change<'a> {
    Change {
        kind,
        action,
        org_id: org,
        repo_id: "",
        subject_kind: SubjectKind::Project,
        subject_id: &p.id,
        payload,
        after,
    }
}

fn project_audit(p: &Project) -> String {
    serde_json::json!({"project": p.number, "title": p.title, "public": p.public, "closed": p.closed}).to_string()
}

fn option(tx: &Tx, name: &str, color: &str) -> Result<ProjectFieldOption> {
    Ok(ProjectFieldOption {
        id: tx.new_id()?,
        name: name.to_string(),
        color: color.to_string(),
        description: String::new(),
    })
}

/// Creates the built-in fields, the `Status` field, the default workflows and the first view.
async fn seed(tx: &mut Tx, p: &Project) -> Result<()> {
    for (name, kind) in BUILT_IN_FIELDS {
        let id = tx.new_id()?;
        let f = ProjectField {
            id: id.clone(),
            project_id: p.id.clone(),
            name: (*name).into(),
            kind: *kind as i32,
            built_in: true,
            created_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        tx.insert(
            &PROJECT_FIELDS,
            &Row::new(p.id.clone(), id).k1(lc(name)).msg(&f),
        )
        .await?;
    }
    let sid = tx.new_id()?;
    let mut options = Vec::new();
    for (n, c) in STATUS_OPTIONS {
        options.push(option(tx, n, c)?);
    }
    let status = ProjectField {
        id: sid.clone(),
        project_id: p.id.clone(),
        name: "Status".into(),
        kind: ProjectFieldKind::SingleSelect as i32,
        options: options.clone(),
        created_at: Some(ts(tx.now_ms())),
        ..Default::default()
    };
    tx.insert(
        &PROJECT_FIELDS,
        &Row::new(p.id.clone(), sid.clone())
            .k1("status")
            .msg(&status),
    )
    .await?;
    let (todo, in_progress, done) = (
        options[0].id.clone(),
        options[1].id.clone(),
        options[2].id.clone(),
    );
    use ProjectWorkflowKind as K;
    let defaults: [(K, &str, bool, &str, &str, &str, u32); 10] = [
        (
            K::ItemAdded,
            "Item added to project",
            true,
            &sid,
            &todo,
            "",
            0,
        ),
        (
            K::ItemReopened,
            "Item reopened",
            true,
            &sid,
            &in_progress,
            "",
            0,
        ),
        (K::ItemClosed, "Item closed", true, &sid, &done, "", 0),
        (K::ChangeLanded, "Change landed", true, &sid, &done, "", 0),
        (
            K::ChangesRequested,
            "Changes requested",
            false,
            &sid,
            &in_progress,
            "",
            0,
        ),
        (
            K::ChangeApproved,
            "Change approved",
            false,
            &sid,
            &done,
            "",
            0,
        ),
        (
            K::AutoArchive,
            "Auto-archive items",
            false,
            "",
            "",
            "is:closed",
            14,
        ),
        (
            K::AutoAdd,
            "Auto-add to project",
            false,
            "",
            "",
            "is:open",
            0,
        ),
        (
            K::AutoCloseIssue,
            "Auto-close issue",
            false,
            &sid,
            &done,
            "",
            0,
        ),
        (
            K::AutoAddSubIssues,
            "Auto-add sub-issues",
            false,
            "",
            "",
            "",
            0,
        ),
    ];
    for (kind, name, enabled, field, opt, filter, days) in defaults {
        let id = tx.new_id()?;
        let w = ProjectWorkflow {
            id: id.clone(),
            project_id: p.id.clone(),
            kind: kind as i32,
            name: name.into(),
            enabled,
            filter: filter.into(),
            field_id: field.into(),
            option_id: opt.into(),
            archive_after_days: days,
            is_default: true,
            updated_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        tx.insert(&WORKFLOWS, &Row::new(p.id.clone(), id).msg(&w))
            .await?;
    }
    let number = next_counter(tx, &p.id, "view_numbers").await?;
    let vid = tx.new_id()?;
    let view = ProjectView {
        id: vid.clone(),
        project_id: p.id.clone(),
        number,
        name: "View 1".into(),
        layout: ProjectViewLayout::Table as i32,
        position: 0,
        created_at: Some(ts(tx.now_ms())),
        ..Default::default()
    };
    tx.insert(
        &VIEWS,
        &Row::new(p.id.clone(), vid).k1(pad(number)).msg(&view),
    )
    .await?;
    Ok(())
}

/// Per-project item sets for `project:` filters on issues: names (lower-cased titles and
/// numbers) to issue ids, and every issue in some project of the org.
pub(crate) async fn issue_project_sets(
    tx: &mut Tx,
    org_id: &str,
) -> Result<(BTreeMap<String, BTreeSet<String>>, BTreeSet<String>)> {
    let mut by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut any = BTreeSet::new();
    for (p, _) in scan_all_msgs::<Project>(tx, &PROJECTS, Scan::scope(org_id, 1)).await? {
        let mut ids = BTreeSet::new();
        for (it, _) in
            scan_all_msgs::<ItemRec>(tx, &PROJECT_ITEMS, Scan::scope(p.id.clone(), 1)).await?
        {
            if let Some(id) = it.content_key.strip_prefix("issue:") {
                ids.insert(id.to_string());
                any.insert(id.to_string());
            }
        }
        by_name
            .entry(lc(&p.title))
            .or_default()
            .extend(ids.iter().cloned());
        by_name.entry(p.number.to_string()).or_default().extend(ids);
    }
    Ok((by_name, any))
}

/// Which projects a list shows.
#[derive(Clone, Debug, Default)]
pub struct ProjectListQuery {
    /// Owner id (set owner or repo).
    pub owner_id: String,
    /// Projects linked to this repo.
    pub repo_id: String,
    /// Title search and `is:` qualifiers.
    pub query: String,
    /// Include closed projects.
    pub include_closed: bool,
    /// Templates only.
    pub templates_only: bool,
    /// Only public projects (the viewer is not a member of the owner).
    pub public_only: bool,
}

/// A copy request.
#[derive(Clone, Debug, Default)]
pub struct CopyRequest {
    /// New project owner.
    pub owner: OwnerInfo,
    /// New title.
    pub title: String,
    /// Copy draft items too.
    pub include_drafts: bool,
    /// New project visibility.
    pub public: bool,
}

impl Work {
    /// Creates a project with its built-in fields, `Status` field, default workflows and
    /// first view.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_project(
        &self,
        cx: &Cx,
        owner: &OwnerInfo,
        actor: &Actor,
        title: &str,
        short_description: &str,
        readme: &str,
        public: bool,
        link_repos: &[RepoInfo],
    ) -> Result<Project> {
        let title = valid_name("title", title, TITLE_MAX)?;
        if short_description.chars().count() > SHORT_MAX {
            return Err(ForgeError::invalid(
                "description_too_long",
                format!("short descriptions are at most {SHORT_MAX} characters"),
            ));
        }
        if readme.len() > limits::BODY {
            return Err(ForgeError::too_large(
                "readme_bytes",
                limits::BODY as u64,
                readme.len() as u64,
            ));
        }
        if owner.kind == ProjectOwnerKind::Unspecified {
            return Err(ForgeError::missing("owner"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let number = next_counter(&mut tx, &owner.id, "project_numbers").await?;
        let id = tx.new_id()?;
        let mut p = Project {
            id: id.clone(),
            owner_id: owner.id.clone(),
            owner_login: owner.login.clone(),
            owner_kind: owner.kind as i32,
            number,
            title,
            short_description: short_description.trim().to_string(),
            readme: readme.to_string(),
            public,
            creator: Some(actor.principal.clone()),
            created_at: Some(ts(tx.now_ms())),
            updated_at: Some(ts(tx.now_ms())),
            version: 1,
            ..Default::default()
        };
        for r in link_repos {
            if owner.kind == ProjectOwnerKind::Org && r.org_id != owner.id {
                return Err(ForgeError::invalid(
                    "different_org",
                    "an org project links only repos of that org",
                ));
            }
            p.linked_repo_ids.push(r.id.clone());
            tx.put(
                &PROJECT_REPOS,
                &Row::new(r.id.clone(), id.clone()).k1(owner.id.clone()),
            )
            .await?;
        }
        p.url = project_url(&p);
        tx.insert(
            &PROJECTS,
            &Row::new(owner.id.clone(), id.clone())
                .k1(pad(number))
                .n1(p.created_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
                .msg(&p),
        )
        .await?;
        register_id(&mut tx, "project", &id, &owner.id).await?;
        seed(&mut tx, &p).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            pchange(
                "project.created",
                "project.create",
                &org,
                &p,
                pjson!(&p),
                project_audit(&p),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// A project by id.
    pub async fn get_project(&self, cx: &Cx, id: &str) -> Result<Project> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(load_project(&mut tx, id, false).await?.0)
    }

    /// A project by owner and number.
    pub async fn get_project_by_number(
        &self,
        cx: &Cx,
        owner_id: &str,
        number: u64,
    ) -> Result<Project> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &PROJECTS,
                &Scan::scope(owner_id, 1).key(Key::K1(pad(number))),
            )
            .await?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| ForgeError::not_found(format!("project #{number}")))?;
        let mut p: Project = row.decode()?;
        p.version = row.version;
        p.url = project_url(&p);
        Ok(p)
    }

    /// An owner's projects (or those linked to a repo), newest first.
    pub async fn list_projects(
        &self,
        cx: &Cx,
        q: &ProjectListQuery,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<Project>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut open_only = !q.include_closed;
        let (mut templates, mut text) = (q.templates_only, Vec::new());
        for w in q.query.split_whitespace() {
            match w.to_lowercase().as_str() {
                "is:open" => open_only = true,
                "is:closed" => open_only = false,
                "is:template" => templates = true,
                "is:public" | "is:private" => {}
                other => text.push(other.to_string()),
            }
        }
        let want_closed = q.query.to_lowercase().contains("is:closed");
        let mut all: Vec<Project> = Vec::new();
        if !q.repo_id.is_empty() {
            for link in scan_all(&mut tx, &PROJECT_REPOS, Scan::scope(q.repo_id.clone(), 1)).await?
            {
                if let Ok((p, _)) = load_project(&mut tx, &link.id, false).await {
                    all.push(p);
                }
            }
        } else {
            for (mut p, r) in
                scan_all_msgs::<Project>(&mut tx, &PROJECTS, Scan::scope(q.owner_id.clone(), 1))
                    .await?
            {
                p.version = r.version;
                p.url = project_url(&p);
                all.push(p);
            }
        }
        all.retain(|p| {
            (!q.public_only || p.public)
                && (!open_only || !p.closed || want_closed)
                && (!want_closed || p.closed)
                && (!templates || p.is_template)
                && text.iter().all(|t| p.title.to_lowercase().contains(t))
        });
        all.sort_by(|a, b| b.number.cmp(&a.number).then(b.id.cmp(&a.id)));
        let rest: Vec<Project> = all
            .into_iter()
            .filter(|p| after.is_empty() || p.id.as_str() < after)
            .collect();
        let next = if rest.len() > limit {
            rest[limit - 1].id.clone()
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }

    /// Edits a project.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_project(
        &self,
        cx: &Cx,
        id: &str,
        title: Option<String>,
        short_description: Option<String>,
        readme: Option<String>,
        public: Option<bool>,
        closed: Option<bool>,
        is_template: Option<bool>,
        expected_version: u64,
    ) -> Result<Project> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, row) = load_project(&mut tx, id, true).await?;
        check_version(expected_version, row.version)?;
        let was_closed = p.closed;
        if let Some(t) = title {
            p.title = valid_name("title", &t, TITLE_MAX)?;
        }
        if let Some(s) = short_description {
            if s.chars().count() > SHORT_MAX {
                return Err(ForgeError::invalid(
                    "description_too_long",
                    format!("short descriptions are at most {SHORT_MAX} characters"),
                ));
            }
            p.short_description = s.trim().to_string();
        }
        if let Some(r) = readme {
            if r.len() > limits::BODY {
                return Err(ForgeError::too_large(
                    "readme_bytes",
                    limits::BODY as u64,
                    r.len() as u64,
                ));
            }
            p.readme = r;
        }
        if let Some(b) = public {
            p.public = b;
        }
        if let Some(b) = is_template {
            p.is_template = b;
        }
        if let Some(c) = closed {
            p.closed = c;
            p.closed_at = if c { Some(ts(tx.now_ms())) } else { None };
        }
        let v = save_project(&mut tx, &mut p, row.version).await?;
        p.version = v;
        let org = org_of(&p);
        let (kind, action) = if closed == Some(true) && !was_closed {
            ("project.closed", "project.close")
        } else {
            ("project.edited", "project.update")
        };
        record(
            &mut tx,
            pchange(kind, action, &org, &p, pjson!(&p), project_audit(&p)),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Deletes a project and everything in it.
    pub async fn delete_project(&self, cx: &Cx, actor: &Actor, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, id, true).await?;
        for (it, row) in
            scan_all_msgs::<ItemRec>(&mut tx, &PROJECT_ITEMS, Scan::scope(id, 1)).await?
        {
            super::items::drop_item_rows(&mut tx, &p, &it, &row.id, &actor.principal, false)
                .await?;
        }
        for t in [
            &PROJECT_FIELDS,
            &VIEWS,
            &WORKFLOWS,
            &CHARTS,
            &STATUS_UPDATES,
            &ITEM_LOG,
        ] {
            for r in scan_all(&mut tx, t, Scan::scope(id, 1)).await? {
                tx.delete(t, id, &r.id).await?;
            }
        }
        for r in &p.linked_repo_ids {
            tx.delete(&PROJECT_REPOS, r, id).await?;
        }
        tx.delete(&PROJECTS, &p.owner_id, id).await?;
        forget_id(&mut tx, "project", id).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            pchange(
                "project.deleted",
                "project.delete",
                &org,
                &p,
                pjson!(&p),
                project_audit(&p),
            ),
        )
        .await?;
        tx.commit().await
    }

    /// Copies a project's structure (fields, views, workflows, charts, optionally drafts)
    /// into a new project.
    pub async fn copy_project(
        &self,
        cx: &Cx,
        actor: &Actor,
        source_id: &str,
        req: CopyRequest,
    ) -> Result<Project> {
        let title = valid_name("title", &req.title, TITLE_MAX)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (src, _) = load_project(&mut tx, source_id, false).await?;
        let number = next_counter(&mut tx, &req.owner.id, "project_numbers").await?;
        let id = tx.new_id()?;
        let mut p = Project {
            id: id.clone(),
            owner_id: req.owner.id.clone(),
            owner_login: req.owner.login.clone(),
            owner_kind: req.owner.kind as i32,
            number,
            title,
            short_description: src.short_description.clone(),
            readme: src.readme.clone(),
            public: req.public,
            creator: Some(actor.principal.clone()),
            copied_from_project_id: src.id.clone(),
            created_at: Some(ts(tx.now_ms())),
            updated_at: Some(ts(tx.now_ms())),
            version: 1,
            ..Default::default()
        };
        p.url = project_url(&p);
        tx.insert(
            &PROJECTS,
            &Row::new(req.owner.id.clone(), id.clone())
                .k1(pad(number))
                .n1(p.created_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
                .msg(&p),
        )
        .await?;
        register_id(&mut tx, "project", &id, &req.owner.id).await?;
        let map = super::views::copy_structure(&mut tx, &src.id, &id).await?;
        if req.include_drafts {
            super::items::copy_drafts(&mut tx, &src.id, &mut p, &map, &actor.principal).await?;
            let (_, row) = load_project(&mut tx, &id, true).await?;
            save_project(&mut tx, &mut p, row.version).await?;
        }
        let org = org_of(&p);
        record(
            &mut tx,
            pchange(
                "project.created",
                "project.copy",
                &org,
                &p,
                pjson!(&p),
                serde_json::json!({"project": p.number, "title": p.title, "copied_from": src.id})
                    .to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Links a repo to a project.
    pub async fn link_project_repo(
        &self,
        cx: &Cx,
        project_id: &str,
        repo: &RepoInfo,
    ) -> Result<Project> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, row) = load_project(&mut tx, project_id, true).await?;
        if p.owner_kind == ProjectOwnerKind::Org as i32 && repo.org_id != p.owner_id {
            return Err(ForgeError::invalid(
                "different_org",
                "an org project links only repos of that org",
            ));
        }
        if p.linked_repo_ids.contains(&repo.id) {
            return Ok(p);
        }
        p.linked_repo_ids.push(repo.id.clone());
        tx.put(
            &PROJECT_REPOS,
            &Row::new(repo.id.clone(), p.id.clone()).k1(p.owner_id.clone()),
        )
        .await?;
        save_project(&mut tx, &mut p, row.version).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            pchange(
                "project.edited",
                "project.link_repo",
                &org,
                &p,
                pjson!(&p),
                serde_json::json!({"project": p.number, "repo": repo.full_name}).to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Unlinks a repo from a project.
    pub async fn unlink_project_repo(
        &self,
        cx: &Cx,
        project_id: &str,
        repo: &RepoInfo,
    ) -> Result<Project> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, row) = load_project(&mut tx, project_id, true).await?;
        if !p.linked_repo_ids.contains(&repo.id) {
            return Ok(p);
        }
        p.linked_repo_ids.retain(|r| *r != repo.id);
        tx.delete(&PROJECT_REPOS, &repo.id, &p.id).await?;
        save_project(&mut tx, &mut p, row.version).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            pchange(
                "project.edited",
                "project.unlink_repo",
                &org,
                &p,
                pjson!(&p),
                serde_json::json!({"project": p.number, "repo": repo.full_name}).to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Links a team (slug or id) with a role. Teams belong to the enterprise module; the link
    /// is recorded as given.
    pub async fn link_project_team(
        &self,
        cx: &Cx,
        project_id: &str,
        team: &str,
        role: ProjectTeamRole,
    ) -> Result<Project> {
        let team = valid_name("team", team, 100)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, row) = load_project(&mut tx, project_id, true).await?;
        let role = if role == ProjectTeamRole::Unspecified {
            ProjectTeamRole::Read
        } else {
            role
        };
        match p
            .linked_teams
            .iter_mut()
            .find(|t| t.team_slug == team || t.team_id == team)
        {
            Some(t) => t.role = role as i32,
            None => p.linked_teams.push(ProjectTeamLink {
                team_id: team.clone(),
                team_slug: team.clone(),
                role: role as i32,
            }),
        }
        save_project(&mut tx, &mut p, row.version).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            pchange(
                "project.edited",
                "project.link_team",
                &org,
                &p,
                pjson!(&p),
                serde_json::json!({"project": p.number, "team": team, "role": role.as_str_name()})
                    .to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Unlinks a team.
    pub async fn unlink_project_team(
        &self,
        cx: &Cx,
        project_id: &str,
        team: &str,
    ) -> Result<Project> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, row) = load_project(&mut tx, project_id, true).await?;
        let before = p.linked_teams.len();
        p.linked_teams
            .retain(|t| t.team_slug != team && t.team_id != team);
        if p.linked_teams.len() == before {
            return Ok(p);
        }
        save_project(&mut tx, &mut p, row.version).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            pchange(
                "project.edited",
                "project.unlink_team",
                &org,
                &p,
                pjson!(&p),
                serde_json::json!({"project": p.number, "team": team}).to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    // ---- status updates ----

    /// Posts a status update; it becomes the project's latest.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_status_update(
        &self,
        cx: &Cx,
        actor: &Actor,
        project_id: &str,
        status: ProjectStatus,
        start_ms: Option<u64>,
        target_ms: Option<u64>,
        body: &str,
    ) -> Result<ProjectStatusUpdate> {
        if body.len() > limits::BODY {
            return Err(ForgeError::too_large(
                "body_bytes",
                limits::BODY as u64,
                body.len() as u64,
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, row) = load_project(&mut tx, project_id, true).await?;
        let id = tx.new_id()?;
        let now = ts(tx.now_ms());
        let u = ProjectStatusUpdate {
            id: id.clone(),
            project_id: project_id.to_string(),
            status: status as i32,
            start_on: start_ms.map(ts),
            target_on: target_ms.map(ts),
            body: body.to_string(),
            creator: Some(actor.principal.clone()),
            created_at: Some(now),
            updated_at: Some(now),
            version: 1,
        };
        tx.insert(&STATUS_UPDATES, &Row::new(project_id, id).msg(&u))
            .await?;
        p.latest_status_update = Some(u.clone());
        save_project(&mut tx, &mut p, row.version).await?;
        let org = org_of(&p);
        record_status(&mut tx, "project_status_update.create", &org, &p, &u).await?;
        tx.commit().await?;
        Ok(u)
    }

    /// Edits a status update.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_status_update(
        &self,
        cx: &Cx,
        project_id: &str,
        id: &str,
        status: ProjectStatus,
        start_ms: Option<u64>,
        target_ms: Option<u64>,
        body: Option<String>,
        expected_version: u64,
    ) -> Result<ProjectStatusUpdate> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, prow) = load_project(&mut tx, project_id, true).await?;
        let row = tx
            .get_for_update(&STATUS_UPDATES, project_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("status update {id}")))?;
        check_version(expected_version, row.version)?;
        let mut u: ProjectStatusUpdate = row.decode()?;
        if status != ProjectStatus::Unspecified {
            u.status = status as i32;
        }
        if let Some(s) = start_ms {
            u.start_on = Some(ts(s));
        }
        if let Some(t) = target_ms {
            u.target_on = Some(ts(t));
        }
        if let Some(b) = body {
            if b.len() > limits::BODY {
                return Err(ForgeError::too_large(
                    "body_bytes",
                    limits::BODY as u64,
                    b.len() as u64,
                ));
            }
            u.body = b;
        }
        u.updated_at = Some(ts(tx.now_ms()));
        let v = tx
            .update(
                &STATUS_UPDATES,
                &Row::new(project_id, id).version(row.version).msg(&u),
            )
            .await?;
        u.version = v;
        if p.latest_status_update
            .as_ref()
            .is_some_and(|l| l.id == u.id)
        {
            p.latest_status_update = Some(u.clone());
            save_project(&mut tx, &mut p, prow.version).await?;
        }
        let org = org_of(&p);
        record_status(&mut tx, "project_status_update.update", &org, &p, &u).await?;
        tx.commit().await?;
        Ok(u)
    }

    /// Deletes a status update; the previous one becomes the latest.
    pub async fn delete_status_update(&self, cx: &Cx, project_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut p, prow) = load_project(&mut tx, project_id, true).await?;
        let (u, _) = tx
            .get_msg::<ProjectStatusUpdate>(&STATUS_UPDATES, project_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("status update {id}")))?;
        tx.delete(&STATUS_UPDATES, project_id, id).await?;
        if p.latest_status_update.as_ref().is_some_and(|l| l.id == id) {
            let rest = scan_all_msgs::<ProjectStatusUpdate>(
                &mut tx,
                &STATUS_UPDATES,
                Scan::scope(project_id, 1),
            )
            .await?;
            p.latest_status_update = rest
                .into_iter()
                .map(|(u, r)| {
                    let mut u = u;
                    u.version = r.version;
                    u
                })
                .max_by(|a, b| a.id.cmp(&b.id));
            save_project(&mut tx, &mut p, prow.version).await?;
        }
        let org = org_of(&p);
        record_status(&mut tx, "project_status_update.delete", &org, &p, &u).await?;
        tx.commit().await
    }

    /// A project's status updates, newest first.
    pub async fn list_status_updates(
        &self,
        cx: &Cx,
        project_id: &str,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<ProjectStatusUpdate>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        let mut all: Vec<ProjectStatusUpdate> = scan_all_msgs::<ProjectStatusUpdate>(
            &mut tx,
            &STATUS_UPDATES,
            Scan::scope(project_id, 1),
        )
        .await?
        .into_iter()
        .map(|(mut u, r)| {
            u.version = r.version;
            u
        })
        .collect();
        all.sort_by(|a, b| b.id.cmp(&a.id));
        let rest: Vec<ProjectStatusUpdate> = all
            .into_iter()
            .filter(|u| after.is_empty() || u.id.as_str() < after)
            .collect();
        let next = if rest.len() > limit {
            rest[limit - 1].id.clone()
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }
}

async fn record_status(
    tx: &mut Tx,
    action: &str,
    org: &str,
    p: &Project,
    u: &ProjectStatusUpdate,
) -> Result<()> {
    record(
        tx,
        pchange(
            "project_status_update.changed",
            action,
            org,
            p,
            pjson!(u),
            serde_json::json!({"project": p.number, "status": u.status, "status_update": u.id})
                .to_string(),
        ),
    )
    .await
}

/// Item activity for historical charts.
pub(crate) async fn log_item(
    tx: &mut Tx,
    project_id: &str,
    item_id: &str,
    kind: i32,
    field_id: &str,
    display: &str,
    number: f64,
) -> Result<()> {
    let id = tx.new_id()?;
    let now = tx.now_ms() as i64;
    let rec = ItemLogRec {
        item_id: item_id.to_string(),
        kind,
        field_id: field_id.to_string(),
        display: display.to_string(),
        number,
    };
    tx.insert(
        &ITEM_LOG,
        &Row::new(project_id, id).k1(item_id).n1(now).msg(&rec),
    )
    .await
}

/// The principal that automation acts as.
pub(crate) fn automation_principal() -> Principal {
    Principal {
        id: "system".into(),
        kind: bee_proto::v1::PrincipalKind::Service as i32,
        display: "project automation".into(),
        ..Default::default()
    }
}
