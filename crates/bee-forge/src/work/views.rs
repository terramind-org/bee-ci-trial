//! Project fields, iterations and saved views.

use std::collections::BTreeMap;

use bee_proto::v1::{
    Project, ProjectChart, ProjectField, ProjectFieldKind, ProjectFieldOption, ProjectFieldValue,
    ProjectIteration, ProjectIterationConfig, ProjectRoadmapZoom, ProjectView, ProjectViewLayout,
    ProjectViewSort, ProjectWorkflow, project_field_value::Value,
};

use super::items::save_item;
use super::model::ItemRec;
use super::projects::{load_fields, load_project, org_of, pchange, status_field};
use super::*;
use crate::ids::{ts, ts_ms};

const DAY_MS: u64 = 86_400_000;
const NAMED_COLORS: [&str; 8] = [
    "GRAY", "BLUE", "GREEN", "YELLOW", "ORANGE", "RED", "PINK", "PURPLE",
];

/// Old id to new id, for copying a project.
#[derive(Default)]
pub struct IdMap(BTreeMap<String, String>);

impl IdMap {
    /// The new id for an old one (unchanged when unknown).
    pub fn id(&self, old: &str) -> String {
        self.0.get(old).cloned().unwrap_or_else(|| old.to_string())
    }

    /// Same, for field ids.
    pub fn field(&self, old: &str) -> String {
        self.id(old)
    }
}

fn kind_of(f: &ProjectField) -> ProjectFieldKind {
    ProjectFieldKind::try_from(f.kind).unwrap_or(ProjectFieldKind::Unspecified)
}

fn valid_option_color(c: &str) -> Result<String> {
    let t = c.trim();
    if t.is_empty() {
        return Ok("GRAY".into());
    }
    let up = t.to_uppercase();
    if NAMED_COLORS.contains(&up.as_str()) {
        return Ok(up);
    }
    valid_color(t, "")
}

/// Checks a value against a project field and normalizes option and iteration references to
/// ids.
pub(crate) fn validate_item_value(
    f: &ProjectField,
    v: &ProjectFieldValue,
) -> Result<ProjectFieldValue> {
    if f.built_in {
        return Err(ForgeError::invalid(
            "built_in_field",
            format!("{:?} is derived from the item and cannot be set", f.name),
        ));
    }
    let bad = |m: String| Err(ForgeError::invalid("invalid_field_value", m));
    let value = match (kind_of(f), &v.value) {
        (ProjectFieldKind::Text, Some(Value::Text(t))) => {
            if t.chars().count() > 1024 {
                return bad(format!("text of {:?} is too long", f.name));
            }
            Value::Text(t.clone())
        }
        (ProjectFieldKind::Number, Some(Value::Number(n))) if n.is_finite() => Value::Number(*n),
        (ProjectFieldKind::Date, Some(Value::DateOn(d))) => Value::DateOn(*d),
        (ProjectFieldKind::SingleSelect, Some(Value::OptionId(o))) => {
            let opt = f
                .options
                .iter()
                .find(|x| x.id == *o || x.name.eq_ignore_ascii_case(o))
                .ok_or_else(|| {
                    ForgeError::invalid(
                        "unknown_option",
                        format!("{:?} has no option {o:?}", f.name),
                    )
                })?;
            Value::OptionId(opt.id.clone())
        }
        (ProjectFieldKind::Iteration, Some(Value::IterationId(i))) => {
            let cfg = f.iteration_config.clone().unwrap_or_default();
            let it = cfg
                .iterations
                .iter()
                .chain(cfg.completed_iterations.iter())
                .find(|x| !x.is_break && (x.id == *i || x.title.eq_ignore_ascii_case(i)))
                .ok_or_else(|| {
                    ForgeError::invalid(
                        "unknown_iteration",
                        format!("{:?} has no iteration {i:?}", f.name),
                    )
                })?;
            Value::IterationId(it.id.clone())
        }
        _ => {
            return bad(format!(
                "the value does not match the kind of field {:?}",
                f.name
            ));
        }
    };
    Ok(ProjectFieldValue {
        field_id: f.id.clone(),
        field_name: f.name.clone(),
        kind: f.kind,
        value: Some(value),
        display_value: String::new(),
    })
}

fn config_change<'a>(action: &'a str, org: &'a str, p: &'a Project, payload: String) -> Change<'a> {
    pchange(
        "project_config.changed",
        action,
        org,
        p,
        payload.clone(),
        payload,
    )
}

fn assign_options(
    tx: &Tx,
    existing: &[ProjectFieldOption],
    new: &[ProjectFieldOption],
) -> Result<Vec<ProjectFieldOption>> {
    let mut out: Vec<ProjectFieldOption> = Vec::new();
    for o in new {
        let name = valid_name("option name", &o.name, 100)?;
        if out.iter().any(|x| x.name.eq_ignore_ascii_case(&name)) {
            return Err(ForgeError::invalid(
                "duplicate_option",
                format!("option {name:?} repeats"),
            ));
        }
        let id = if !o.id.is_empty() && existing.iter().any(|e| e.id == o.id) {
            o.id.clone()
        } else {
            tx.new_id()?
        };
        out.push(ProjectFieldOption {
            id,
            name,
            color: valid_option_color(&o.color)?,
            description: o.description.clone(),
        });
    }
    if out.len() > limits::OPTIONS {
        return Err(exhausted("field_options", limits::OPTIONS, out.len()));
    }
    Ok(out)
}

fn assign_iterations(
    tx: &Tx,
    existing: &ProjectIterationConfig,
    new: &ProjectIterationConfig,
    field_id: &str,
    default_days: u32,
) -> Result<ProjectIterationConfig> {
    let all_existing: Vec<&ProjectIteration> = existing
        .iterations
        .iter()
        .chain(existing.completed_iterations.iter())
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut conv = |src: &[ProjectIteration], completed: bool| -> Result<Vec<ProjectIteration>> {
        let mut out = Vec::new();
        for i in src {
            let title = if i.is_break && i.title.trim().is_empty() {
                "Break".to_string()
            } else {
                valid_name("iteration title", &i.title, 100)?
            };
            let id = if !i.id.is_empty() && all_existing.iter().any(|e| e.id == i.id) {
                i.id.clone()
            } else {
                tx.new_id()?
            };
            if !seen.insert(id.clone()) {
                return Err(ForgeError::invalid(
                    "duplicate_iteration",
                    "an iteration appears twice",
                ));
            }
            let days = if i.duration_days == 0 {
                default_days
            } else {
                i.duration_days
            };
            if days > 365 {
                return Err(ForgeError::invalid(
                    "invalid_duration",
                    "iterations last at most 365 days",
                ));
            }
            out.push(ProjectIteration {
                id,
                field_id: field_id.to_string(),
                title,
                start_on: i.start_on,
                duration_days: days,
                completed: completed || i.completed,
                is_break: i.is_break,
            });
        }
        Ok(out)
    };
    let iterations = conv(&new.iterations, false)?;
    let completed_iterations = conv(&new.completed_iterations, true)?;
    if iterations.len() + completed_iterations.len() > 200 {
        return Err(exhausted(
            "iterations",
            200,
            iterations.len() + completed_iterations.len(),
        ));
    }
    Ok(ProjectIterationConfig {
        start_on: new.start_on,
        duration_days: default_days,
        iterations,
        completed_iterations,
    })
}

/// Clears matching values from every item of a project (the items are saved without moving
/// their update time).
async fn purge_item_values(
    tx: &mut Tx,
    project_id: &str,
    drop: impl Fn(&ProjectFieldValue) -> bool,
) -> Result<()> {
    for (rec, row) in
        scan_all_msgs::<ItemRec>(tx, &PROJECT_ITEMS, Scan::scope(project_id, 1)).await?
    {
        if rec
            .item
            .as_ref()
            .is_some_and(|i| i.field_values.iter().any(&drop))
        {
            let (mut rec, _) = (rec, ());
            if let Some(i) = rec.item.as_mut() {
                i.field_values.retain(|v| !drop(v));
            }
            save_item(tx, project_id, &mut rec, row.version, false).await?;
        }
    }
    Ok(())
}

/// Copies fields, views, workflows and charts of `src` into the empty project `dst`.
pub(crate) async fn copy_structure(tx: &mut Tx, src: &str, dst: &str) -> Result<IdMap> {
    let mut map = IdMap::default();
    let fields = load_fields(tx, src).await?;
    for f in &fields {
        map.0.insert(f.id.clone(), tx.new_id()?);
        for o in &f.options {
            map.0.insert(o.id.clone(), tx.new_id()?);
        }
        if let Some(c) = &f.iteration_config {
            for i in c.iterations.iter().chain(c.completed_iterations.iter()) {
                map.0.insert(i.id.clone(), tx.new_id()?);
            }
        }
    }
    for f in fields {
        let mut n = f.clone();
        n.id = map.id(&f.id);
        n.project_id = dst.to_string();
        n.created_at = Some(ts(tx.now_ms()));
        for o in &mut n.options {
            o.id = map.id(&o.id);
        }
        if let Some(c) = n.iteration_config.as_mut() {
            for i in c
                .iterations
                .iter_mut()
                .chain(c.completed_iterations.iter_mut())
            {
                i.id = map.id(&i.id);
                i.field_id = n.id.clone();
            }
        }
        let row = Row::new(dst, n.id.clone()).k1(lc(&n.name)).msg(&n);
        tx.insert(&PROJECT_FIELDS, &row).await?;
    }
    let views = scan_all_msgs::<ProjectView>(tx, &VIEWS, Scan::scope(src, 1)).await?;
    for (v, _) in views {
        let number = next_counter(tx, dst, "view_numbers").await?;
        let mut n = v.clone();
        n.id = tx.new_id()?;
        n.project_id = dst.to_string();
        n.number = number;
        n.created_at = Some(ts(tx.now_ms()));
        n.group_by_field_ids = v.group_by_field_ids.iter().map(|x| map.id(x)).collect();
        n.column_field_id = map.id(&v.column_field_id);
        n.slice_by_field_id = map.id(&v.slice_by_field_id);
        n.sort_by = v
            .sort_by
            .iter()
            .map(|s| ProjectViewSort {
                field_id: map.id(&s.field_id),
                descending: s.descending,
            })
            .collect();
        n.visible_field_ids = v.visible_field_ids.iter().map(|x| map.id(x)).collect();
        n.sum_field_ids = v.sum_field_ids.iter().map(|x| map.id(x)).collect();
        n.start_date_field_id = map.id(&v.start_date_field_id);
        n.target_date_field_id = map.id(&v.target_date_field_id);
        tx.insert(&VIEWS, &Row::new(dst, n.id.clone()).k1(pad(number)).msg(&n))
            .await?;
    }
    for (w, _) in scan_all_msgs::<ProjectWorkflow>(tx, &WORKFLOWS, Scan::scope(src, 1)).await? {
        let mut n = w.clone();
        n.id = tx.new_id()?;
        n.project_id = dst.to_string();
        n.field_id = map.id(&w.field_id);
        n.option_id = map.id(&w.option_id);
        n.source_repo = None;
        if w.kind == bee_proto::v1::ProjectWorkflowKind::AutoAdd as i32 {
            n.enabled = false;
        }
        tx.insert(&WORKFLOWS, &Row::new(dst, n.id.clone()).msg(&n))
            .await?;
    }
    for (c, _) in scan_all_msgs::<ProjectChart>(tx, &CHARTS, Scan::scope(src, 1)).await? {
        let mut n = c.clone();
        n.id = tx.new_id()?;
        n.project_id = dst.to_string();
        n.x_axis_field_id = map.id(&c.x_axis_field_id);
        n.group_by_field_id = map.id(&c.group_by_field_id);
        n.y_axis_field_id = map.id(&c.y_axis_field_id);
        n.version = 1;
        tx.insert(&CHARTS, &Row::new(dst, n.id.clone()).msg(&n))
            .await?;
    }
    Ok(map)
}

/// A view by id or decimal number.
pub(crate) async fn find_view(
    tx: &mut Tx,
    project_id: &str,
    id_or_number: &str,
) -> Result<ProjectView> {
    let missing = || ForgeError::not_found(format!("view {id_or_number}"));
    if let Some((mut v, row)) = tx
        .get_msg::<ProjectView>(&VIEWS, project_id, id_or_number)
        .await?
    {
        v.version = row.version;
        return Ok(v);
    }
    if let Ok(n) = id_or_number.parse::<u64>() {
        let rows = tx
            .scan(&VIEWS, &Scan::scope(project_id, 1).key(Key::K1(pad(n))))
            .await?;
        if let Some(row) = rows.into_iter().next() {
            let mut v: ProjectView = row.decode()?;
            v.version = row.version;
            return Ok(v);
        }
    }
    Err(missing())
}

fn check_view_fields(fields: &[ProjectField], v: &ProjectView) -> Result<()> {
    let find = |id: &str, what: &str| -> Result<&ProjectField> {
        fields.iter().find(|f| f.id == id).ok_or_else(|| {
            ForgeError::invalid(
                "unknown_field",
                format!("{what} names no field of this project: {id}"),
            )
        })
    };
    for id in &v.group_by_field_ids {
        find(id, "group_by_field_ids")?;
    }
    if !v.column_field_id.is_empty() {
        let f = find(&v.column_field_id, "column_field_id")?;
        if !matches!(
            kind_of(f),
            ProjectFieldKind::SingleSelect | ProjectFieldKind::Iteration
        ) {
            return Err(ForgeError::invalid(
                "invalid_column_field",
                "board columns need a single-select or iteration field",
            ));
        }
    }
    if !v.slice_by_field_id.is_empty() {
        find(&v.slice_by_field_id, "slice_by_field_id")?;
    }
    for s in &v.sort_by {
        find(&s.field_id, "sort_by")?;
    }
    for id in &v.visible_field_ids {
        find(id, "visible_field_ids")?;
    }
    for id in &v.sum_field_ids {
        let f = find(id, "sum_field_ids")?;
        if kind_of(f) != ProjectFieldKind::Number {
            return Err(ForgeError::invalid(
                "invalid_sum_field",
                "only number fields can be summed",
            ));
        }
    }
    for (id, what) in [
        (&v.start_date_field_id, "start_date_field_id"),
        (&v.target_date_field_id, "target_date_field_id"),
    ] {
        if !id.is_empty() {
            let f = find(id, what)?;
            if !matches!(
                kind_of(f),
                ProjectFieldKind::Date | ProjectFieldKind::Iteration
            ) {
                return Err(ForgeError::invalid(
                    "invalid_date_field",
                    "roadmap dates need a date or iteration field",
                ));
            }
        }
    }
    Ok(())
}

/// Everything a view can hold; ids are the project's field ids.
#[derive(Clone, Debug, Default)]
pub struct ViewSpec {
    /// Tab name.
    pub name: String,
    /// Layout (unspecified = table).
    pub layout: ProjectViewLayout,
    /// Filter text.
    pub filter: String,
    /// Grouping fields.
    pub group_by_field_ids: Vec<String>,
    /// Board column field.
    pub column_field_id: String,
    /// Slice field.
    pub slice_by_field_id: String,
    /// Sort keys.
    pub sort_by: Vec<ProjectViewSort>,
    /// Visible fields in order.
    pub visible_field_ids: Vec<String>,
    /// Summed number fields.
    pub sum_field_ids: Vec<String>,
    /// Roadmap start field.
    pub start_date_field_id: String,
    /// Roadmap target field.
    pub target_date_field_id: String,
    /// Roadmap zoom.
    pub roadmap_zoom: ProjectRoadmapZoom,
}

/// A partial view edit (`None` = unchanged).
#[derive(Clone, Debug, Default)]
pub struct ViewPatch {
    /// New name.
    pub name: Option<String>,
    /// New layout (unspecified = unchanged).
    pub layout: ProjectViewLayout,
    /// New filter.
    pub filter: Option<String>,
    /// New grouping.
    pub group_by: Option<Vec<String>>,
    /// New board column field.
    pub column_field_id: Option<String>,
    /// New slice field.
    pub slice_by_field_id: Option<String>,
    /// New sort.
    pub sort_by: Option<Vec<ProjectViewSort>>,
    /// New visible fields.
    pub visible_field_ids: Option<Vec<String>>,
    /// New summed fields.
    pub sum_field_ids: Option<Vec<String>>,
    /// New roadmap start field.
    pub start_date_field_id: Option<String>,
    /// New roadmap target field.
    pub target_date_field_id: Option<String>,
    /// New zoom (unspecified = unchanged).
    pub roadmap_zoom: ProjectRoadmapZoom,
    /// New tab position.
    pub position: Option<u32>,
}

impl Work {
    // ---- fields ----

    /// Adds a custom field.
    pub async fn create_project_field(
        &self,
        cx: &Cx,
        project_id: &str,
        name: &str,
        kind: ProjectFieldKind,
        options: &[ProjectFieldOption],
        config: Option<&ProjectIterationConfig>,
    ) -> Result<ProjectField> {
        let name = valid_name("name", name, 100)?;
        if !matches!(
            kind,
            ProjectFieldKind::Text
                | ProjectFieldKind::Number
                | ProjectFieldKind::Date
                | ProjectFieldKind::SingleSelect
                | ProjectFieldKind::Iteration
        ) {
            return Err(ForgeError::invalid(
                "invalid_field_kind",
                "custom fields are TEXT, NUMBER, DATE, SINGLE_SELECT or ITERATION",
            ));
        }
        if kind != ProjectFieldKind::SingleSelect && !options.is_empty() {
            return Err(ForgeError::invalid(
                "options_not_allowed",
                "options belong to single-select fields",
            ));
        }
        if kind == ProjectFieldKind::SingleSelect && options.is_empty() {
            return Err(ForgeError::invalid(
                "options_required",
                "a single-select field needs options",
            ));
        }
        if kind != ProjectFieldKind::Iteration && config.is_some_and(|c| !c.iterations.is_empty()) {
            return Err(ForgeError::invalid(
                "iterations_not_allowed",
                "iterations belong to iteration fields",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        if tx
            .count(&PROJECT_FIELDS, project_id, Some(&Key::K1(lc(&name))))
            .await?
            > 0
        {
            return Err(ForgeError::already_exists(format!("field {name:?}")));
        }
        let count = tx.count(&PROJECT_FIELDS, project_id, None).await? as usize;
        if count >= limits::PROJECT_FIELDS + super::projects::BUILT_IN_FIELDS.len() {
            return Err(exhausted(
                "project_fields",
                limits::PROJECT_FIELDS,
                count + 1,
            ));
        }
        let id = tx.new_id()?;
        let mut f = ProjectField {
            id: id.clone(),
            project_id: project_id.to_string(),
            name: name.clone(),
            kind: kind as i32,
            built_in: false,
            options: assign_options(&tx, &[], options)?,
            iteration_config: None,
            created_at: Some(ts(tx.now_ms())),
            version: 1,
        };
        if kind == ProjectFieldKind::Iteration {
            let c = config.cloned().unwrap_or_default();
            let days = if c.duration_days == 0 {
                14
            } else {
                c.duration_days
            };
            if days > 365 {
                return Err(ForgeError::invalid(
                    "invalid_duration",
                    "iterations last at most 365 days",
                ));
            }
            let mut cfg =
                assign_iterations(&tx, &ProjectIterationConfig::default(), &c, &id, days)?;
            if cfg.start_on.is_none() {
                cfg.start_on = Some(ts(tx.now_ms()));
            }
            f.iteration_config = Some(cfg);
        }
        tx.insert(
            &PROJECT_FIELDS,
            &Row::new(project_id, id).k1(lc(&name)).msg(&f),
        )
        .await?;
        let org = org_of(&p);
        record(
            &mut tx,
            config_change(
                "project_config.create_field",
                &org,
                &p,
                serde_json::json!({"project": p.number, "field": name, "kind": kind.as_str_name()})
                    .to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(f)
    }

    /// Edits a custom field.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_project_field(
        &self,
        cx: &Cx,
        project_id: &str,
        field_id: &str,
        name: Option<String>,
        options: Option<&[ProjectFieldOption]>,
        config: Option<&ProjectIterationConfig>,
        expected_version: u64,
    ) -> Result<ProjectField> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let row = tx
            .get_for_update(&PROJECT_FIELDS, project_id, field_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("project field {field_id}")))?;
        check_version(expected_version, row.version)?;
        let mut f: ProjectField = row.decode()?;
        if f.built_in {
            return Err(ForgeError::invalid(
                "built_in_field",
                "built-in fields cannot be edited",
            ));
        }
        if let Some(n) = name {
            let n = valid_name("name", &n, 100)?;
            let clash = tx
                .scan(
                    &PROJECT_FIELDS,
                    &Scan::scope(project_id, 1).key(Key::K1(lc(&n))),
                )
                .await?
                .into_iter()
                .any(|r| r.id != f.id);
            if clash {
                return Err(ForgeError::already_exists(format!("field {n:?}")));
            }
            f.name = n;
        }
        let mut removed: Vec<String> = Vec::new();
        if let Some(o) = options {
            if kind_of(&f) != ProjectFieldKind::SingleSelect {
                return Err(ForgeError::invalid(
                    "options_not_allowed",
                    "options belong to single-select fields",
                ));
            }
            if o.is_empty() {
                return Err(ForgeError::invalid(
                    "options_required",
                    "a single-select field needs options",
                ));
            }
            let new = assign_options(&tx, &f.options, o)?;
            removed.extend(
                f.options
                    .iter()
                    .filter(|e| !new.iter().any(|n| n.id == e.id))
                    .map(|e| e.id.clone()),
            );
            f.options = new;
        }
        if let Some(c) = config {
            if kind_of(&f) != ProjectFieldKind::Iteration {
                return Err(ForgeError::invalid(
                    "iterations_not_allowed",
                    "iterations belong to iteration fields",
                ));
            }
            let old = f.iteration_config.clone().unwrap_or_default();
            let days = if c.duration_days == 0 {
                old.duration_days.max(1)
            } else {
                c.duration_days
            };
            let new = assign_iterations(&tx, &old, c, &f.id, days)?;
            let keep: Vec<&str> = new
                .iterations
                .iter()
                .chain(new.completed_iterations.iter())
                .map(|i| i.id.as_str())
                .collect();
            removed.extend(
                old.iterations
                    .iter()
                    .chain(old.completed_iterations.iter())
                    .filter(|e| !keep.contains(&e.id.as_str()))
                    .map(|e| e.id.clone()),
            );
            f.iteration_config = Some(ProjectIterationConfig {
                start_on: c.start_on.or(old.start_on),
                ..new
            });
        }
        let v = tx
            .update(
                &PROJECT_FIELDS,
                &Row::new(project_id, field_id)
                    .k1(lc(&f.name))
                    .version(row.version)
                    .msg(&f),
            )
            .await?;
        f.version = v;
        if !removed.is_empty() {
            let fid = f.id.clone();
            purge_item_values(&mut tx, project_id, |v| {
                v.field_id == fid
                    && match &v.value {
                        Some(Value::OptionId(o)) => removed.contains(o),
                        Some(Value::IterationId(i)) => removed.contains(i),
                        _ => false,
                    }
            })
            .await?;
        }
        let org = org_of(&p);
        record(
            &mut tx,
            config_change(
                "project_config.update_field",
                &org,
                &p,
                serde_json::json!({"project": p.number, "field": f.name}).to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(f)
    }

    /// Deletes a custom field: its values go, views stop referencing it and workflows that
    /// set or watch it are disabled.
    pub async fn delete_project_field(
        &self,
        cx: &Cx,
        project_id: &str,
        field_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (f, _) = tx
            .get_msg::<ProjectField>(&PROJECT_FIELDS, project_id, field_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("project field {field_id}")))?;
        if f.built_in {
            return Err(ForgeError::invalid(
                "built_in_field",
                "built-in fields cannot be deleted",
            ));
        }
        purge_item_values(&mut tx, project_id, |v| v.field_id == field_id).await?;
        tx.delete(&PROJECT_FIELDS, project_id, field_id).await?;
        for (mut v, row) in
            scan_all_msgs::<ProjectView>(&mut tx, &VIEWS, Scan::scope(project_id, 1)).await?
        {
            let before = v.clone();
            v.group_by_field_ids.retain(|x| x != field_id);
            v.sort_by.retain(|s| s.field_id != field_id);
            v.visible_field_ids.retain(|x| x != field_id);
            v.sum_field_ids.retain(|x| x != field_id);
            for s in [
                &mut v.column_field_id,
                &mut v.slice_by_field_id,
                &mut v.start_date_field_id,
                &mut v.target_date_field_id,
            ] {
                if s == field_id {
                    s.clear();
                }
            }
            if v != before {
                tx.update(
                    &VIEWS,
                    &Row::new(project_id, v.id.clone())
                        .k1(pad(v.number))
                        .version(row.version)
                        .msg(&v),
                )
                .await?;
            }
        }
        for (mut w, row) in
            scan_all_msgs::<ProjectWorkflow>(&mut tx, &WORKFLOWS, Scan::scope(project_id, 1))
                .await?
        {
            if w.field_id == field_id {
                w.field_id.clear();
                w.option_id.clear();
                w.enabled = false;
                tx.update(
                    &WORKFLOWS,
                    &Row::new(project_id, w.id.clone())
                        .version(row.version)
                        .msg(&w),
                )
                .await?;
            }
        }
        for (mut c, row) in
            scan_all_msgs::<ProjectChart>(&mut tx, &CHARTS, Scan::scope(project_id, 1)).await?
        {
            let before = c.clone();
            for s in [
                &mut c.x_axis_field_id,
                &mut c.group_by_field_id,
                &mut c.y_axis_field_id,
            ] {
                if s == field_id {
                    s.clear();
                }
            }
            if c != before {
                tx.update(
                    &CHARTS,
                    &Row::new(project_id, c.id.clone())
                        .version(row.version)
                        .msg(&c),
                )
                .await?;
            }
        }
        let org = org_of(&p);
        record(
            &mut tx,
            config_change(
                "project_config.delete_field",
                &org,
                &p,
                serde_json::json!({"project": p.number, "field": f.name}).to_string(),
            ),
        )
        .await?;
        tx.commit().await
    }

    /// A project's fields in creation order.
    pub async fn list_project_fields(
        &self,
        cx: &Cx,
        project_id: &str,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<ProjectField>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        let all = load_fields(&mut tx, project_id).await?;
        let rest: Vec<ProjectField> = all
            .into_iter()
            .filter(|f| after.is_empty() || f.id.as_str() > after)
            .collect();
        let next = if rest.len() > limit {
            rest[limit - 1].id.clone()
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }

    // ---- iterations ----

    async fn iteration_field(
        tx: &mut Tx,
        project_id: &str,
        field_id: &str,
        lock: bool,
    ) -> Result<(ProjectField, Row)> {
        let row = if lock {
            tx.get_for_update(&PROJECT_FIELDS, project_id, field_id)
                .await?
        } else {
            tx.get(&PROJECT_FIELDS, project_id, field_id).await?
        }
        .ok_or_else(|| ForgeError::not_found(format!("project field {field_id}")))?;
        let f: ProjectField = row.decode()?;
        if kind_of(&f) != ProjectFieldKind::Iteration {
            return Err(ForgeError::invalid(
                "not_an_iteration_field",
                format!("{:?} is not an iteration field", f.name),
            ));
        }
        Ok((f, row))
    }

    /// The iterations of a field: active and upcoming first, then completed ones.
    pub async fn list_iterations(
        &self,
        cx: &Cx,
        project_id: &str,
        field_id: &str,
        include_completed: bool,
    ) -> Result<Vec<ProjectIteration>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        let (f, _) = Self::iteration_field(&mut tx, project_id, field_id, false).await?;
        let cfg = f.iteration_config.unwrap_or_default();
        let mut out = cfg.iterations;
        if include_completed {
            out.extend(cfg.completed_iterations);
        }
        Ok(out)
    }

    /// Appends an iteration (or break) after the last one.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_iteration(
        &self,
        cx: &Cx,
        project_id: &str,
        field_id: &str,
        title: &str,
        start_ms: Option<u64>,
        duration_days: u32,
        is_break: bool,
    ) -> Result<ProjectIteration> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (mut f, row) = Self::iteration_field(&mut tx, project_id, field_id, true).await?;
        let mut cfg = f.iteration_config.clone().unwrap_or_default();
        let total = cfg.iterations.len() + cfg.completed_iterations.len();
        if total >= 200 {
            return Err(exhausted("iterations", 200, total + 1));
        }
        let days = if duration_days == 0 {
            cfg.duration_days.max(1)
        } else {
            duration_days
        };
        if days > 365 {
            return Err(ForgeError::invalid(
                "invalid_duration",
                "iterations last at most 365 days",
            ));
        }
        let end_of_last = cfg
            .iterations
            .iter()
            .chain(cfg.completed_iterations.iter())
            .map(|i| {
                i.start_on.as_ref().map(ts_ms).unwrap_or(0) + u64::from(i.duration_days) * DAY_MS
            })
            .max();
        let start = start_ms
            .or(end_of_last)
            .or_else(|| cfg.start_on.as_ref().map(ts_ms))
            .unwrap_or_else(|| tx.now_ms());
        let title = if title.trim().is_empty() {
            if is_break {
                "Break".to_string()
            } else {
                format!("Iteration {}", total + 1)
            }
        } else {
            valid_name("title", title, 100)?
        };
        let it = ProjectIteration {
            id: tx.new_id()?,
            field_id: field_id.to_string(),
            title,
            start_on: Some(ts(start)),
            duration_days: days,
            completed: false,
            is_break,
        };
        cfg.iterations.push(it.clone());
        f.iteration_config = Some(cfg);
        tx.update(
            &PROJECT_FIELDS,
            &Row::new(project_id, field_id)
                .k1(lc(&f.name))
                .version(row.version)
                .msg(&f),
        )
        .await?;
        let org = org_of(&p);
        record(
            &mut tx,
            config_change(
                "project_config.add_iteration",
                &org,
                &p,
                serde_json::json!({"project": p.number, "field": f.name, "iteration": it.title})
                    .to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(it)
    }

    /// Completes an iteration, optionally moving its open items to another one.
    pub async fn complete_iteration(
        &self,
        cx: &Cx,
        project_id: &str,
        field_id: &str,
        iteration_id: &str,
        move_to: &str,
    ) -> Result<ProjectIteration> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (mut f, row) = Self::iteration_field(&mut tx, project_id, field_id, true).await?;
        let mut cfg = f.iteration_config.clone().unwrap_or_default();
        let pos = cfg
            .iterations
            .iter()
            .position(|i| i.id == iteration_id)
            .ok_or_else(|| ForgeError::not_found(format!("iteration {iteration_id}")))?;
        if cfg.iterations[pos].is_break {
            return Err(ForgeError::invalid(
                "is_break",
                "a break cannot be completed",
            ));
        }
        if !move_to.is_empty() {
            let ok = cfg
                .iterations
                .iter()
                .any(|i| i.id == move_to && !i.is_break)
                && move_to != iteration_id;
            if !ok {
                return Err(ForgeError::invalid(
                    "invalid_target_iteration",
                    "the target must be another active or upcoming iteration",
                ));
            }
        }
        let mut done = cfg.iterations.remove(pos);
        done.completed = true;
        cfg.completed_iterations.push(done.clone());
        f.iteration_config = Some(cfg);
        tx.update(
            &PROJECT_FIELDS,
            &Row::new(project_id, field_id)
                .k1(lc(&f.name))
                .version(row.version)
                .msg(&f),
        )
        .await?;
        if !move_to.is_empty() {
            for (mut rec, irow) in
                scan_all_msgs::<ItemRec>(&mut tx, &PROJECT_ITEMS, Scan::scope(project_id, 1))
                    .await?
            {
                let open = rec
                    .item
                    .as_ref()
                    .is_some_and(|i| i.content_state == "open" || i.content_state == "draft");
                let mut moved = false;
                if let Some(it) = rec.item.as_mut()
                    && open
                {
                    for v in &mut it.field_values {
                        if v.field_id == field_id
                            && v.value == Some(Value::IterationId(iteration_id.to_string()))
                        {
                            v.value = Some(Value::IterationId(move_to.to_string()));
                            moved = true;
                        }
                    }
                }
                if moved {
                    save_item(&mut tx, project_id, &mut rec, irow.version, true).await?;
                }
            }
        }
        let org = org_of(&p);
        record(&mut tx, config_change("project_config.complete_iteration", &org, &p, serde_json::json!({"project": p.number, "field": f.name, "iteration": done.title, "moved_to": move_to}).to_string())).await?;
        tx.commit().await?;
        Ok(done)
    }

    // ---- views ----

    /// Creates a saved view.
    pub async fn create_view(
        &self,
        cx: &Cx,
        project_id: &str,
        spec: ViewSpec,
    ) -> Result<ProjectView> {
        let name = valid_name("name", &spec.name, 100)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let count = tx.count(&VIEWS, project_id, None).await? as usize;
        if count >= limits::VIEWS {
            return Err(exhausted("project_views", limits::VIEWS, count + 1));
        }
        if !spec.filter.trim().is_empty() {
            super::pfilter::Compiled::new(&spec.filter, &fields)?;
        }
        let number = next_counter(&mut tx, project_id, "view_numbers").await?;
        let layout = if spec.layout == ProjectViewLayout::Unspecified {
            ProjectViewLayout::Table
        } else {
            spec.layout
        };
        let mut v = ProjectView {
            id: tx.new_id()?,
            project_id: project_id.to_string(),
            number,
            name,
            layout: layout as i32,
            filter: spec.filter.trim().to_string(),
            group_by_field_ids: spec.group_by_field_ids,
            column_field_id: spec.column_field_id,
            slice_by_field_id: spec.slice_by_field_id,
            sort_by: spec.sort_by,
            visible_field_ids: spec.visible_field_ids,
            sum_field_ids: spec.sum_field_ids,
            start_date_field_id: spec.start_date_field_id,
            target_date_field_id: spec.target_date_field_id,
            roadmap_zoom: if layout == ProjectViewLayout::Roadmap
                && spec.roadmap_zoom == ProjectRoadmapZoom::Unspecified
            {
                ProjectRoadmapZoom::Month as i32
            } else {
                spec.roadmap_zoom as i32
            },
            position: count as u32,
            created_at: Some(ts(tx.now_ms())),
            version: 1,
        };
        if layout == ProjectViewLayout::Board && v.column_field_id.is_empty() {
            v.column_field_id = status_field(&fields).map(|f| f.id.clone()).ok_or_else(|| {
                ForgeError::invalid("column_field_required", "a board needs `column_field_id`")
            })?;
        }
        check_view_fields(&fields, &v)?;
        tx.insert(
            &VIEWS,
            &Row::new(project_id, v.id.clone()).k1(pad(number)).msg(&v),
        )
        .await?;
        let org = org_of(&p);
        record(&mut tx, config_change("project_config.create_view", &org, &p, serde_json::json!({"project": p.number, "view": v.name, "layout": layout.as_str_name()}).to_string())).await?;
        tx.commit().await?;
        Ok(v)
    }

    /// One view by id or number.
    pub async fn get_view(
        &self,
        cx: &Cx,
        project_id: &str,
        id_or_number: &str,
    ) -> Result<ProjectView> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        find_view(&mut tx, project_id, id_or_number).await
    }

    /// Edits a view.
    pub async fn update_view(
        &self,
        cx: &Cx,
        project_id: &str,
        id_or_number: &str,
        patch: ViewPatch,
        expected_version: u64,
    ) -> Result<ProjectView> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let found = find_view(&mut tx, project_id, id_or_number).await?;
        let row = tx
            .get_for_update(&VIEWS, project_id, &found.id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("view {id_or_number}")))?;
        check_version(expected_version, row.version)?;
        let mut v: ProjectView = row.decode()?;
        if let Some(n) = patch.name {
            v.name = valid_name("name", &n, 100)?;
        }
        if patch.layout != ProjectViewLayout::Unspecified {
            v.layout = patch.layout as i32;
        }
        if let Some(f) = patch.filter {
            if !f.trim().is_empty() {
                super::pfilter::Compiled::new(&f, &fields)?;
            }
            v.filter = f.trim().to_string();
        }
        if let Some(g) = patch.group_by {
            v.group_by_field_ids = g;
        }
        if let Some(c) = patch.column_field_id {
            v.column_field_id = c;
        }
        if let Some(s) = patch.slice_by_field_id {
            v.slice_by_field_id = s;
        }
        if let Some(s) = patch.sort_by {
            v.sort_by = s;
        }
        if let Some(x) = patch.visible_field_ids {
            v.visible_field_ids = x;
        }
        if let Some(x) = patch.sum_field_ids {
            v.sum_field_ids = x;
        }
        if let Some(x) = patch.start_date_field_id {
            v.start_date_field_id = x;
        }
        if let Some(x) = patch.target_date_field_id {
            v.target_date_field_id = x;
        }
        if patch.roadmap_zoom != ProjectRoadmapZoom::Unspecified {
            v.roadmap_zoom = patch.roadmap_zoom as i32;
        }
        if let Some(pos) = patch.position {
            v.position = pos;
        }
        if v.layout == ProjectViewLayout::Board as i32 && v.column_field_id.is_empty() {
            v.column_field_id = status_field(&fields).map(|f| f.id.clone()).ok_or_else(|| {
                ForgeError::invalid("column_field_required", "a board needs `column_field_id`")
            })?;
        }
        check_view_fields(&fields, &v)?;
        let nv = tx
            .update(
                &VIEWS,
                &Row::new(project_id, v.id.clone())
                    .k1(pad(v.number))
                    .version(row.version)
                    .msg(&v),
            )
            .await?;
        v.version = nv;
        let org = org_of(&p);
        record(
            &mut tx,
            config_change(
                "project_config.update_view",
                &org,
                &p,
                serde_json::json!({"project": p.number, "view": v.name}).to_string(),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(v)
    }

    /// Deletes a view.
    pub async fn delete_view(&self, cx: &Cx, project_id: &str, id_or_number: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let v = find_view(&mut tx, project_id, id_or_number).await?;
        tx.delete(&VIEWS, project_id, &v.id).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            config_change(
                "project_config.delete_view",
                &org,
                &p,
                serde_json::json!({"project": p.number, "view": v.name}).to_string(),
            ),
        )
        .await?;
        tx.commit().await
    }

    /// A project's views in tab order.
    pub async fn list_views(
        &self,
        cx: &Cx,
        project_id: &str,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<ProjectView>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        let mut all: Vec<ProjectView> =
            scan_all_msgs::<ProjectView>(&mut tx, &VIEWS, Scan::scope(project_id, 1))
                .await?
                .into_iter()
                .map(|(mut v, r)| {
                    v.version = r.version;
                    v
                })
                .collect();
        all.sort_by_key(|v| (v.position, v.number));
        let key = |v: &ProjectView| format!("{:010}:{:012}", v.position, v.number);
        let rest: Vec<ProjectView> = all
            .into_iter()
            .filter(|v| after.is_empty() || key(v).as_str() > after)
            .collect();
        let next = if rest.len() > limit {
            key(&rest[limit - 1])
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }
}
