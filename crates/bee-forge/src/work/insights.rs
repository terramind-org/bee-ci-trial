//! Project insights: saved charts and their evaluation. Current charts bucket the items by a
//! field; historical charts replay the project's item activity over time.

use std::collections::BTreeMap;

use bee_proto::v1::{
    Project, ProjectChart, ProjectChartAggregate, ProjectChartLayout, ProjectChartPoint,
    ProjectChartSeries, ProjectField, ProjectFieldKind, project_field_value::Value,
};

use super::items::display_of;
use super::model::{ItemLogRec, ItemRec, log_kind};
use super::pfilter::{Compiled, EvalCtx};
use super::projects::{load_fields, load_project, org_of, pchange};
use super::*;
use crate::ids::{ts, ts_ms};

const DAY_MS: u64 = 86_400_000;
const MAX_POINTS: u64 = 366;

fn agg_of(c: &ProjectChart) -> ProjectChartAggregate {
    match ProjectChartAggregate::try_from(c.aggregate).unwrap_or(ProjectChartAggregate::Unspecified)
    {
        ProjectChartAggregate::Unspecified => ProjectChartAggregate::Count,
        a => a,
    }
}

fn validate_chart(c: &ProjectChart, fields: &[ProjectField]) -> Result<()> {
    let find = |id: &str, what: &str| -> Result<&ProjectField> {
        fields.iter().find(|f| f.id == id).ok_or_else(|| {
            ForgeError::invalid(
                "unknown_field",
                format!("{what} names no field of this project: {id}"),
            )
        })
    };
    if !c.historical && c.x_axis_field_id.is_empty() {
        return Err(ForgeError::missing("x_axis_field_id"));
    }
    if !c.x_axis_field_id.is_empty() {
        find(&c.x_axis_field_id, "x_axis_field_id")?;
    }
    if !c.group_by_field_id.is_empty() {
        find(&c.group_by_field_id, "group_by_field_id")?;
    }
    if agg_of(c) != ProjectChartAggregate::Count {
        if c.y_axis_field_id.is_empty() {
            return Err(ForgeError::missing("y_axis_field_id"));
        }
        let f = find(&c.y_axis_field_id, "y_axis_field_id")?;
        if f.kind != ProjectFieldKind::Number as i32 {
            return Err(ForgeError::invalid(
                "invalid_y_axis",
                "the y axis needs a number field",
            ));
        }
    }
    if !c.filter.trim().is_empty() {
        Compiled::new(&c.filter, fields)?;
    }
    if let (Some(s), Some(e)) = (&c.start_on, &c.end_on)
        && ts_ms(s) > ts_ms(e)
    {
        return Err(ForgeError::invalid(
            "invalid_range",
            "`start_on` is after `end_on`",
        ));
    }
    Ok(())
}

/// The bucket labels an item has for a field (several for assignees and labels).
fn labels_of(rec: &ItemRec, f: &ProjectField) -> Vec<String> {
    let none = || vec![format!("No {}", f.name)];
    let it = rec.item.clone().unwrap_or_default();
    let nonempty = |v: Vec<String>| if v.is_empty() { none() } else { v };
    if f.built_in {
        return match ProjectFieldKind::try_from(f.kind).unwrap_or(ProjectFieldKind::Unspecified) {
            ProjectFieldKind::Title => vec![it.title.clone()],
            ProjectFieldKind::Assignees => nonempty(
                it.assignees
                    .iter()
                    .map(|a| {
                        if a.display.is_empty() {
                            a.id.clone()
                        } else {
                            a.display.clone()
                        }
                    })
                    .collect(),
            ),
            ProjectFieldKind::Labels => nonempty(rec.labels.clone()),
            ProjectFieldKind::Milestone => nonempty(if rec.milestone.is_empty() {
                vec![]
            } else {
                vec![rec.milestone.clone()]
            }),
            ProjectFieldKind::Repository => nonempty(if it.repo_full_name.is_empty() {
                vec![]
            } else {
                vec![it.repo_full_name.clone()]
            }),
            ProjectFieldKind::IssueType => nonempty(if rec.issue_type.is_empty() {
                vec![]
            } else {
                vec![rec.issue_type.clone()]
            }),
            ProjectFieldKind::ParentIssue => nonempty(if rec.parent.is_empty() {
                vec![]
            } else {
                vec![rec.parent.clone()]
            }),
            ProjectFieldKind::Reviewers => nonempty(rec.reviewers.clone()),
            _ => none(),
        };
    }
    match it.field_values.iter().find(|v| v.field_id == f.id) {
        Some(v) => {
            let d = display_of(f, v);
            if d.is_empty() { none() } else { vec![d] }
        }
        None => none(),
    }
}

/// Sort position of a bucket label for a field: option order, iteration order, else name.
fn label_rank(f: &ProjectField, label: &str) -> (u8, usize, String) {
    if label.starts_with("No ") && label == format!("No {}", f.name) {
        return (2, 0, String::new());
    }
    if let Some(i) = f.options.iter().position(|o| o.name == label) {
        return (0, i, String::new());
    }
    if let Some(c) = &f.iteration_config
        && let Some(i) = c
            .iterations
            .iter()
            .chain(c.completed_iterations.iter())
            .position(|x| x.title == label)
    {
        return (0, i, String::new());
    }
    (1, 0, label.to_lowercase())
}

fn y_value(rec: &ItemRec, y: &str) -> Option<f64> {
    rec.item
        .as_ref()?
        .field_values
        .iter()
        .find(|v| v.field_id == y)
        .and_then(|v| match v.value {
            Some(Value::Number(n)) => Some(n),
            _ => None,
        })
}

#[derive(Default, Clone)]
struct Acc {
    n: u64,
    ys: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Acc {
    fn add(&mut self, y: Option<f64>) {
        self.n += 1;
        if let Some(y) = y {
            if self.ys == 0 {
                self.min = y;
                self.max = y;
            }
            self.ys += 1;
            self.sum += y;
            self.min = self.min.min(y);
            self.max = self.max.max(y);
        }
    }

    fn value(&self, a: ProjectChartAggregate) -> Option<f64> {
        match a {
            ProjectChartAggregate::Count | ProjectChartAggregate::Unspecified => {
                Some(self.n as f64)
            }
            ProjectChartAggregate::Sum => Some(self.sum),
            ProjectChartAggregate::Average => (self.ys > 0).then(|| self.sum / self.ys as f64),
            ProjectChartAggregate::Min => (self.ys > 0).then_some(self.min),
            ProjectChartAggregate::Max => (self.ys > 0).then_some(self.max),
        }
    }
}

fn series_from(
    cells: BTreeMap<(String, String), Acc>,
    agg: ProjectChartAggregate,
    xf: Option<&ProjectField>,
    gf: Option<&ProjectField>,
) -> Vec<ProjectChartSeries> {
    let mut groups: Vec<String> = cells.keys().map(|(_, g)| g.clone()).collect();
    groups.sort();
    groups.dedup();
    if let Some(f) = gf {
        groups.sort_by_key(|g| label_rank(f, g));
    }
    let mut xs: Vec<String> = cells.keys().map(|(x, _)| x.clone()).collect();
    xs.sort();
    xs.dedup();
    if let Some(f) = xf {
        xs.sort_by_key(|x| label_rank(f, x));
    }
    let zero_fill = matches!(
        agg,
        ProjectChartAggregate::Count
            | ProjectChartAggregate::Sum
            | ProjectChartAggregate::Unspecified
    );
    groups
        .into_iter()
        .map(|g| {
            let points = xs
                .iter()
                .filter_map(|x| {
                    let v = cells
                        .get(&(x.clone(), g.clone()))
                        .and_then(|a| a.value(agg));
                    let v = if zero_fill { Some(v.unwrap_or(0.0)) } else { v };
                    v.map(|value| ProjectChartPoint {
                        label: x.clone(),
                        at: None,
                        value,
                    })
                })
                .collect();
            ProjectChartSeries { name: g, points }
        })
        .collect()
}

fn chart_change<'a>(action: &'a str, org: &'a str, p: &'a Project, c: &ProjectChart) -> Change<'a> {
    pchange(
        "project_config.changed",
        action,
        org,
        p,
        pjson!(c),
        serde_json::json!({"project": p.number, "chart": c.name}).to_string(),
    )
}

impl Work {
    /// Saves a chart definition.
    pub async fn create_chart(
        &self,
        cx: &Cx,
        project_id: &str,
        chart: ProjectChart,
    ) -> Result<ProjectChart> {
        let name = valid_name("name", &chart.name, 100)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let count = tx.count(&CHARTS, project_id, None).await? as usize;
        if count >= limits::CHARTS {
            return Err(exhausted("project_charts", limits::CHARTS, count + 1));
        }
        let mut c = chart;
        c.id = tx.new_id()?;
        c.project_id = project_id.to_string();
        c.name = name;
        c.version = 1;
        if c.layout == ProjectChartLayout::Unspecified as i32 {
            c.layout = ProjectChartLayout::Column as i32;
        }
        validate_chart(&c, &fields)?;
        tx.insert(&CHARTS, &Row::new(project_id, c.id.clone()).msg(&c))
            .await?;
        let org = org_of(&p);
        record(
            &mut tx,
            chart_change("project_config.create_chart", &org, &p, &c),
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    /// Replaces a chart definition.
    pub async fn update_chart(
        &self,
        cx: &Cx,
        project_id: &str,
        chart_id: &str,
        chart: ProjectChart,
        expected_version: u64,
    ) -> Result<ProjectChart> {
        let name = valid_name("name", &chart.name, 100)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let row = tx
            .get_for_update(&CHARTS, project_id, chart_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("chart {chart_id}")))?;
        check_version(expected_version, row.version)?;
        let mut c = chart;
        c.id = chart_id.to_string();
        c.project_id = project_id.to_string();
        c.name = name;
        if c.layout == ProjectChartLayout::Unspecified as i32 {
            c.layout = ProjectChartLayout::Column as i32;
        }
        validate_chart(&c, &fields)?;
        c.version = tx
            .update(
                &CHARTS,
                &Row::new(project_id, chart_id).version(row.version).msg(&c),
            )
            .await?;
        let org = org_of(&p);
        record(
            &mut tx,
            chart_change("project_config.update_chart", &org, &p, &c),
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    /// Deletes a chart.
    pub async fn delete_chart(&self, cx: &Cx, project_id: &str, chart_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let (c, _) = tx
            .get_msg::<ProjectChart>(&CHARTS, project_id, chart_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("chart {chart_id}")))?;
        tx.delete(&CHARTS, project_id, chart_id).await?;
        let org = org_of(&p);
        record(
            &mut tx,
            chart_change("project_config.delete_chart", &org, &p, &c),
        )
        .await?;
        tx.commit().await
    }

    /// A project's charts.
    pub async fn list_charts(&self, cx: &Cx, project_id: &str) -> Result<Vec<ProjectChart>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        load_project(&mut tx, project_id, false).await?;
        let mut v: Vec<ProjectChart> =
            scan_all_msgs::<ProjectChart>(&mut tx, &CHARTS, Scan::scope(project_id, 1))
                .await?
                .into_iter()
                .map(|(mut c, r)| {
                    c.version = r.version;
                    c
                })
                .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(v)
    }

    /// Evaluates a saved chart or an unsaved definition.
    pub async fn query_chart(
        &self,
        cx: &Cx,
        project_id: &str,
        chart_id: &str,
        definition: Option<ProjectChart>,
        ctx: &EvalCtx,
    ) -> Result<(Vec<ProjectChartSeries>, u64)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (p, _) = load_project(&mut tx, project_id, false).await?;
        let fields = load_fields(&mut tx, project_id).await?;
        let chart = if !chart_id.is_empty() {
            tx.get_msg::<ProjectChart>(&CHARTS, project_id, chart_id)
                .await?
                .map(|(c, _)| c)
                .ok_or_else(|| ForgeError::not_found(format!("chart {chart_id}")))?
        } else {
            let mut c = definition.ok_or_else(|| ForgeError::missing("chart_id"))?;
            c.project_id = project_id.to_string();
            validate_chart(&c, &fields)?;
            c
        };
        let agg = agg_of(&chart);
        let filter = if chart.filter.trim().is_empty() {
            None
        } else {
            Some(Compiled::new(&chart.filter, &fields)?)
        };
        let items: Vec<(ItemRec, String)> =
            scan_all_msgs::<ItemRec>(&mut tx, &PROJECT_ITEMS, Scan::scope(project_id, 1))
                .await?
                .into_iter()
                .map(|(r, row)| (r, row.id))
                .collect();
        let xf = fields.iter().find(|f| f.id == chart.x_axis_field_id);
        let gf = fields.iter().find(|f| f.id == chart.group_by_field_id);
        let now = tx.now_ms();
        if !chart.historical {
            let mut cells: BTreeMap<(String, String), Acc> = BTreeMap::new();
            for (rec, _) in &items {
                if rec.item.as_ref().is_some_and(|i| i.archived) {
                    continue;
                }
                if filter.as_ref().is_some_and(|f| !f.matches(rec, ctx)) {
                    continue;
                }
                let xs = xf
                    .map(|f| labels_of(rec, f))
                    .unwrap_or_else(|| vec![String::new()]);
                let gs = gf
                    .map(|f| labels_of(rec, f))
                    .unwrap_or_else(|| vec![String::new()]);
                let y = y_value(rec, &chart.y_axis_field_id);
                for x in &xs {
                    for g in &gs {
                        cells.entry((x.clone(), g.clone())).or_default().add(y);
                    }
                }
            }
            return Ok((series_from(cells, agg, xf, gf), now));
        }
        // Historical: replay the item activity.
        let start = chart
            .start_on
            .as_ref()
            .map(ts_ms)
            .unwrap_or_else(|| p.created_at.as_ref().map(ts_ms).unwrap_or(now));
        let end = chart.end_on.as_ref().map(ts_ms).unwrap_or(now).max(start);
        let span_days = ((end - start) / DAY_MS).max(1);
        let step = span_days.div_ceil(MAX_POINTS - 1).max(1) * DAY_MS;
        let mut times: Vec<u64> = (0..)
            .map(|i| start + i * step)
            .take_while(|t| *t < end)
            .collect();
        times.push(end);
        let log =
            scan_all_msgs::<ItemLogRec>(&mut tx, &ITEM_LOG, Scan::scope(project_id, 1).by_n1())
                .await?;
        let keep: std::collections::BTreeSet<String> = items
            .iter()
            .filter(|(r, _)| filter.as_ref().is_none_or(|f| f.matches(r, ctx)))
            .map(|(_, id)| id.clone())
            .collect();
        let live: std::collections::BTreeSet<String> =
            items.iter().map(|(_, id)| id.clone()).collect();
        #[derive(Default)]
        struct St {
            present: bool,
            group: Option<String>,
            y: Option<f64>,
        }
        let mut state: BTreeMap<String, St> = BTreeMap::new();
        let mut by_group: BTreeMap<String, Vec<ProjectChartPoint>> = BTreeMap::new();
        let mut i = 0;
        let mut all_groups: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut snapshots: Vec<(u64, BTreeMap<String, Acc>)> = Vec::new();
        for t in &times {
            while i < log.len() && (log[i].1.n1.unwrap_or(0) as u64) <= *t {
                let l = &log[i].0;
                let s = state.entry(l.item_id.clone()).or_default();
                match l.kind {
                    log_kind::ADDED | log_kind::RESTORED => s.present = true,
                    log_kind::REMOVED => s.present = false,
                    log_kind::SET => {
                        if l.field_id == chart.group_by_field_id
                            && !chart.group_by_field_id.is_empty()
                        {
                            s.group = Some(l.display.clone());
                        }
                        if l.field_id == chart.y_axis_field_id && !chart.y_axis_field_id.is_empty()
                        {
                            s.y = Some(l.number);
                        }
                    }
                    log_kind::CLEARED => {
                        if l.field_id == chart.group_by_field_id {
                            s.group = None;
                        }
                        if l.field_id == chart.y_axis_field_id {
                            s.y = None;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let mut cells: BTreeMap<String, Acc> = BTreeMap::new();
            for (id, s) in &state {
                if !s.present || (live.contains(id) && !keep.contains(id)) {
                    continue;
                }
                let g = match (&chart.group_by_field_id.is_empty(), &s.group) {
                    (true, _) => String::new(),
                    (false, Some(g)) => g.clone(),
                    (false, None) => {
                        format!("No {}", gf.map(|f| f.name.as_str()).unwrap_or("value"))
                    }
                };
                all_groups.insert(g.clone());
                cells.entry(g).or_default().add(s.y);
            }
            snapshots.push((*t, cells));
        }
        if all_groups.is_empty() {
            all_groups.insert(String::new());
        }
        let zero_fill = matches!(
            agg,
            ProjectChartAggregate::Count | ProjectChartAggregate::Sum
        );
        for (t, cells) in &snapshots {
            for g in &all_groups {
                let v = cells.get(g).and_then(|a| a.value(agg));
                let v = if zero_fill { Some(v.unwrap_or(0.0)) } else { v };
                if let Some(value) = v {
                    by_group
                        .entry(g.clone())
                        .or_default()
                        .push(ProjectChartPoint {
                            label: String::new(),
                            at: Some(ts(*t)),
                            value,
                        });
                }
            }
        }
        let mut names: Vec<String> = all_groups.into_iter().collect();
        if let Some(f) = gf {
            names.sort_by_key(|g| label_rank(f, g));
        }
        let series = names
            .into_iter()
            .map(|g| ProjectChartSeries {
                points: by_group.remove(&g).unwrap_or_default(),
                name: g,
            })
            .collect();
        Ok((series, now))
    }
}
