//! Org-defined issue types and typed custom issue fields.

use bee_proto::v1::{
    Issue, IssueField, IssueFieldIteration, IssueFieldKind, IssueFieldOption, IssueFieldValue,
    IssueTimelineEventKind as Tk, IssueType, SubjectKind, issue_field_value::Value,
};

use super::issues::{Cache, add_timeline, load_issue, render, save_issue};
use super::model::IssueRec;
use super::*;
use crate::ids::{ts, ts_ms};

const TYPE_COLOR: &str = "6e7781";

/// `YYYY-MM-DD` of a unix-millisecond instant (UTC).
pub(crate) fn iso_date(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn type_audit(t: &IssueType) -> String {
    serde_json::json!({"type": t.name, "enabled": t.enabled}).to_string()
}

fn type_change<'a>(action: &'a str, org_id: &'a str, t: &IssueType) -> Change<'a> {
    Change {
        kind: "issue_type.changed",
        action,
        org_id,
        repo_id: "",
        subject_kind: SubjectKind::Org,
        subject_id: org_id,
        payload: serde_json::json!({
            "action": action.rsplit('.').next().unwrap_or(""),
            "issue_type": serde_json::from_str::<serde_json::Value>(&pjson!(t)).unwrap_or_default(),
        })
        .to_string(),
        after: type_audit(t),
    }
}

/// Resolves a type name or id; with `require_enabled`, a disabled type is rejected.
pub(crate) async fn resolve_type(
    tx: &mut Tx,
    org_id: &str,
    name_or_id: &str,
    require_enabled: bool,
) -> Result<String> {
    let t = find_type(tx, org_id, name_or_id).await?.ok_or_else(|| {
        ForgeError::invalid(
            "unknown_issue_type",
            format!("no issue type {name_or_id:?}"),
        )
    })?;
    if require_enabled && !t.enabled {
        return Err(ForgeError::invalid(
            "issue_type_disabled",
            format!("issue type {:?} is disabled", t.name),
        ));
    }
    Ok(t.id)
}

async fn find_type(tx: &mut Tx, org_id: &str, name_or_id: &str) -> Result<Option<IssueType>> {
    if let Some((t, _)) = tx.get_msg::<IssueType>(&TYPES, org_id, name_or_id).await? {
        return Ok(Some(t));
    }
    Ok(tx
        .scan_msgs::<IssueType>(&TYPES, &Scan::scope(org_id, 1).key(Key::K1(lc(name_or_id))))
        .await?
        .into_iter()
        .next()
        .map(|(t, _)| t))
}

async fn find_field(tx: &mut Tx, org_id: &str, id_or_name: &str) -> Result<Option<IssueField>> {
    if let Some((f, _)) = tx
        .get_msg::<IssueField>(&FIELDS, org_id, id_or_name)
        .await?
    {
        return Ok(Some(f));
    }
    Ok(tx
        .scan_msgs::<IssueField>(
            &FIELDS,
            &Scan::scope(org_id, 1).key(Key::K1(lc(id_or_name))),
        )
        .await?
        .into_iter()
        .next()
        .map(|(f, _)| f))
}

/// What a value displays as.
pub(crate) fn display_value(f: &IssueField, v: &IssueFieldValue) -> String {
    match &v.value {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Number(n)) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Some(Value::DateOn(d)) => iso_date(ts_ms(d)),
        Some(Value::OptionId(o)) => f
            .options
            .iter()
            .find(|x| x.id == *o)
            .map(|x| x.name.clone())
            .unwrap_or_default(),
        Some(Value::IterationId(i)) => f
            .iterations
            .iter()
            .find(|x| x.id == *i)
            .map(|x| x.title.clone())
            .unwrap_or_default(),
        None => String::new(),
    }
}

fn kind_of(f: &IssueField) -> IssueFieldKind {
    IssueFieldKind::try_from(f.kind).unwrap_or(IssueFieldKind::Unspecified)
}

fn normalize_value(f: &IssueField, v: &IssueFieldValue) -> Result<IssueFieldValue> {
    let bad = |m: String| Err(ForgeError::invalid("invalid_field_value", m));
    let value = match (kind_of(f), &v.value) {
        (IssueFieldKind::Text, Some(Value::Text(t))) => {
            if t.chars().count() > 1024 {
                return bad(format!("text of {:?} is too long", f.name));
            }
            Value::Text(t.clone())
        }
        (IssueFieldKind::Number, Some(Value::Number(n))) => {
            if !n.is_finite() {
                return bad(format!("{:?} needs a finite number", f.name));
            }
            Value::Number(*n)
        }
        (IssueFieldKind::Date, Some(Value::DateOn(d))) => Value::DateOn(*d),
        (IssueFieldKind::SingleSelect, Some(Value::OptionId(o))) => {
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
        (IssueFieldKind::Iteration, Some(Value::IterationId(i))) => {
            let it = f
                .iterations
                .iter()
                .find(|x| x.id == *i || x.title.eq_ignore_ascii_case(i))
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
    Ok(IssueFieldValue {
        field_id: f.id.clone(),
        field_name: f.name.clone(),
        kind: f.kind,
        value: Some(value),
    })
}

/// Validates `new` values against the org's fields and merges them into `existing`
/// (a value replaces the existing value of the same field).
pub(crate) async fn validate_values(
    tx: &mut Tx,
    org_id: &str,
    issue_type_id: &str,
    existing: &[IssueFieldValue],
    new: &[IssueFieldValue],
) -> Result<Vec<IssueFieldValue>> {
    let mut out: Vec<IssueFieldValue> = existing.to_vec();
    for v in new {
        let key = if v.field_id.is_empty() {
            &v.field_name
        } else {
            &v.field_id
        };
        if key.is_empty() {
            return Err(ForgeError::missing("field_id"));
        }
        let f = find_field(tx, org_id, key).await?.ok_or_else(|| {
            ForgeError::invalid("unknown_field", format!("no issue field {key:?}"))
        })?;
        if !f.issue_type_ids.is_empty() && !f.issue_type_ids.iter().any(|t| t == issue_type_id) {
            return Err(ForgeError::invalid(
                "field_not_applicable",
                format!("field {:?} does not apply to this issue's type", f.name),
            ));
        }
        let nv = normalize_value(&f, v)?;
        out.retain(|e| e.field_id != nv.field_id);
        out.push(nv);
    }
    Ok(out)
}

/// Removes values matching `drop` from every issue of the org.
async fn purge_values(
    tx: &mut Tx,
    org_id: &str,
    drop: impl Fn(&IssueFieldValue) -> bool,
) -> Result<()> {
    let rows = scan_all_msgs::<IssueRec>(
        tx,
        &ISSUES,
        Scan {
            key: Some(Key::K2(org_id.to_string())),
            ..Scan::default()
        },
    )
    .await?;
    for (mut rec, row) in rows {
        if rec.field_values.iter().any(&drop) {
            let old = rec.clone();
            rec.field_values.retain(|v| !drop(v));
            save_issue(tx, Some(&old), &mut rec, row.version).await?;
        }
    }
    Ok(())
}

fn assign_option_ids(
    tx: &Tx,
    existing: &[IssueFieldOption],
    new: &[IssueFieldOption],
) -> Result<Vec<IssueFieldOption>> {
    let mut out: Vec<IssueFieldOption> = Vec::new();
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
        out.push(IssueFieldOption {
            id,
            name,
            color: valid_color(&o.color, TYPE_COLOR)?,
            description: o.description.clone(),
        });
    }
    if out.len() > limits::OPTIONS {
        return Err(exhausted("field_options", limits::OPTIONS, out.len()));
    }
    Ok(out)
}

fn assign_iteration_ids(
    tx: &Tx,
    existing: &[IssueFieldIteration],
    new: &[IssueFieldIteration],
) -> Result<Vec<IssueFieldIteration>> {
    let mut out: Vec<IssueFieldIteration> = Vec::new();
    for i in new {
        let title = valid_name("iteration title", &i.title, 100)?;
        if out.iter().any(|x| x.title.eq_ignore_ascii_case(&title)) {
            return Err(ForgeError::invalid(
                "duplicate_iteration",
                format!("iteration {title:?} repeats"),
            ));
        }
        let id = if !i.id.is_empty() && existing.iter().any(|e| e.id == i.id) {
            i.id.clone()
        } else {
            tx.new_id()?
        };
        out.push(IssueFieldIteration {
            id,
            title,
            start_on: i.start_on,
            duration_days: i.duration_days,
            completed: i.completed,
        });
    }
    if out.len() > limits::OPTIONS {
        return Err(exhausted("field_iterations", limits::OPTIONS, out.len()));
    }
    Ok(out)
}

fn field_change<'a>(action: &'a str, org_id: &'a str, f: &IssueField) -> Change<'a> {
    Change {
        kind: "issue_field.changed",
        action,
        org_id,
        repo_id: "",
        subject_kind: SubjectKind::Org,
        subject_id: org_id,
        payload: serde_json::json!({
            "action": action.rsplit('.').next().unwrap_or(""),
            "issue_field": serde_json::from_str::<serde_json::Value>(&pjson!(f)).unwrap_or_default(),
        })
        .to_string(),
        after: serde_json::json!({"field": f.name, "kind": f.kind}).to_string(),
    }
}

impl Work {
    // ---- issue types ----

    /// Defines an org issue type.
    pub async fn create_issue_type(
        &self,
        cx: &Cx,
        org_id: &str,
        name: &str,
        description: &str,
        color: &str,
        enabled: bool,
    ) -> Result<IssueType> {
        let name = valid_name("name", name, 50)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if find_type(&mut tx, org_id, &name).await?.is_some() {
            return Err(ForgeError::already_exists(format!("issue type {name:?}")));
        }
        let count = tx.count(&TYPES, org_id, None).await? as usize;
        if count >= limits::TYPES {
            return Err(exhausted("issue_types", limits::TYPES, count + 1));
        }
        let id = tx.new_id()?;
        let t = IssueType {
            id: id.clone(),
            org_id: org_id.to_string(),
            name: name.clone(),
            description: description.trim().to_string(),
            color: valid_color(color, TYPE_COLOR)?,
            enabled,
            position: count as u32,
            created_at: Some(ts(tx.now_ms())),
            version: 1,
        };
        tx.insert(&TYPES, &Row::new(org_id, id).k1(lc(&name)).msg(&t))
            .await?;
        record(&mut tx, type_change("issue_type.create", org_id, &t)).await?;
        tx.commit().await?;
        Ok(t)
    }

    /// Edits an org issue type.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_issue_type(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        name: Option<String>,
        description: Option<String>,
        color: Option<String>,
        enabled: Option<bool>,
        position: Option<u32>,
        expected_version: u64,
    ) -> Result<IssueType> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let row = tx
            .get_for_update(&TYPES, org_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("issue type {id}")))?;
        let mut t: IssueType = row.decode()?;
        check_version(expected_version, row.version)?;
        if let Some(n) = name {
            let n = valid_name("name", &n, 50)?;
            if let Some(o) = find_type(&mut tx, org_id, &n).await?
                && o.id != t.id
            {
                return Err(ForgeError::already_exists(format!("issue type {n:?}")));
            }
            t.name = n;
        }
        if let Some(d) = description {
            t.description = d.trim().to_string();
        }
        if let Some(c) = color {
            t.color = valid_color(&c, TYPE_COLOR)?;
        }
        if let Some(e) = enabled {
            t.enabled = e;
        }
        if let Some(p) = position {
            t.position = p;
        }
        t.version = row.version + 1;
        let v = tx
            .update(
                &TYPES,
                &Row::new(org_id, id)
                    .k1(lc(&t.name))
                    .version(row.version)
                    .msg(&t),
            )
            .await?;
        t.version = v;
        record(&mut tx, type_change("issue_type.update", org_id, &t)).await?;
        tx.commit().await?;
        Ok(t)
    }

    /// Deletes a type; issues of that type become untyped.
    pub async fn delete_issue_type(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = tx
            .get_msg::<IssueType>(&TYPES, org_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("issue type {id}")))?;
        let rows = scan_all_msgs::<IssueRec>(
            &mut tx,
            &ISSUES,
            Scan {
                key: Some(Key::K2(org_id.to_string())),
                ..Scan::default()
            },
        )
        .await?;
        for (mut rec, row) in rows {
            if rec.issue_type_id == id {
                let old = rec.clone();
                rec.issue_type_id.clear();
                save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
            }
        }
        tx.delete(&TYPES, org_id, id).await?;
        record(&mut tx, type_change("issue_type.delete", org_id, &t)).await?;
        tx.commit().await
    }

    /// An org's types in display order.
    pub async fn list_issue_types(
        &self,
        cx: &Cx,
        org_id: &str,
        include_disabled: bool,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<IssueType>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut all: Vec<IssueType> =
            scan_all_msgs::<IssueType>(&mut tx, &TYPES, Scan::scope(org_id, 1))
                .await?
                .into_iter()
                .map(|(mut t, r)| {
                    t.version = r.version;
                    t
                })
                .filter(|t| include_disabled || t.enabled)
                .collect();
        all.sort_by_key(|t| (t.position, t.id.clone()));
        let rest: Vec<IssueType> = all
            .into_iter()
            .filter(|t| after.is_empty() || format!("{:010}:{}", t.position, t.id).as_str() > after)
            .collect();
        let next = if rest.len() > limit {
            let t = &rest[limit - 1];
            format!("{:010}:{}", t.position, t.id)
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }

    /// Sets or clears an issue's type.
    pub async fn set_issue_type(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        issue_type: &str,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let new_id = if issue_type.trim().is_empty() {
            String::new()
        } else {
            resolve_type(&mut tx, &repo.org_id, issue_type, true).await?
        };
        if new_id == rec.issue_type_id {
            return render(&mut tx, &mut cache, &rec, row.version).await;
        }
        let old = rec.clone();
        let prev = if rec.issue_type_id.is_empty() {
            String::new()
        } else {
            tx.get_msg::<IssueType>(&TYPES, &repo.org_id, &rec.issue_type_id)
                .await?
                .map(|(t, _)| t.name)
                .unwrap_or_default()
        };
        let cur = if new_id.is_empty() {
            String::new()
        } else {
            tx.get_msg::<IssueType>(&TYPES, &repo.org_id, &new_id)
                .await?
                .map(|(t, _)| t.name)
                .unwrap_or_default()
        };
        // Values of fields that do not apply to the new type are dropped.
        let fields =
            scan_all_msgs::<IssueField>(&mut tx, &FIELDS, Scan::scope(repo.org_id.clone(), 1))
                .await?;
        rec.field_values.retain(|v| {
            fields
                .iter()
                .find(|(f, _)| f.id == v.field_id)
                .is_some_and(|(f, _)| {
                    f.issue_type_ids.is_empty() || f.issue_type_ids.contains(&new_id)
                })
        });
        rec.issue_type_id = new_id;
        add_timeline(&mut tx, &rec.id, Tk::TypeChanged, &actor.principal, |e| {
            e.previous_value = prev;
            e.current_value = cur;
        })
        .await?;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            Change {
                kind: "issue.typed",
                action: "issue.set_type",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id: &rec.id,
                payload: pjson!(&issue),
                after: serde_json::json!({"number": rec.number, "type": issue_type}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    // ---- issue fields ----

    /// Defines a custom issue field.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_issue_field(
        &self,
        cx: &Cx,
        org_id: &str,
        name: &str,
        description: &str,
        kind: IssueFieldKind,
        options: &[IssueFieldOption],
        iterations: &[IssueFieldIteration],
        issue_type_ids: &[String],
    ) -> Result<IssueField> {
        let name = valid_name("name", name, 50)?;
        if kind == IssueFieldKind::Unspecified {
            return Err(ForgeError::missing("kind"));
        }
        if kind != IssueFieldKind::SingleSelect && !options.is_empty() {
            return Err(ForgeError::invalid(
                "options_not_allowed",
                "options belong to single-select fields",
            ));
        }
        if kind != IssueFieldKind::Iteration && !iterations.is_empty() {
            return Err(ForgeError::invalid(
                "iterations_not_allowed",
                "iterations belong to iteration fields",
            ));
        }
        if kind == IssueFieldKind::SingleSelect && options.is_empty() {
            return Err(ForgeError::invalid(
                "options_required",
                "a single-select field needs options",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if find_field(&mut tx, org_id, &name).await?.is_some() {
            return Err(ForgeError::already_exists(format!("issue field {name:?}")));
        }
        let count = tx.count(&FIELDS, org_id, None).await? as usize;
        if count >= limits::FIELDS {
            return Err(exhausted("issue_fields", limits::FIELDS, count + 1));
        }
        let mut types = Vec::new();
        for t in issue_type_ids {
            types.push(resolve_type(&mut tx, org_id, t, false).await?);
        }
        let id = tx.new_id()?;
        let f = IssueField {
            id: id.clone(),
            org_id: org_id.to_string(),
            name: name.clone(),
            description: description.trim().to_string(),
            kind: kind as i32,
            options: assign_option_ids(&tx, &[], options)?,
            iterations: assign_iteration_ids(&tx, &[], iterations)?,
            issue_type_ids: types,
            created_at: Some(ts(tx.now_ms())),
            version: 1,
        };
        tx.insert(&FIELDS, &Row::new(org_id, id).k1(lc(&name)).msg(&f))
            .await?;
        record(&mut tx, field_change("issue_field.create", org_id, &f)).await?;
        tx.commit().await?;
        Ok(f)
    }

    /// Edits a field; the kind cannot change. Values naming removed options or iterations
    /// are cleared.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_issue_field(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        name: Option<String>,
        description: Option<String>,
        options: Option<&[IssueFieldOption]>,
        iterations: Option<&[IssueFieldIteration]>,
        issue_type_ids: Option<&[String]>,
        expected_version: u64,
    ) -> Result<IssueField> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let row = tx
            .get_for_update(&FIELDS, org_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("issue field {id}")))?;
        let mut f: IssueField = row.decode()?;
        check_version(expected_version, row.version)?;
        if let Some(n) = name {
            let n = valid_name("name", &n, 50)?;
            if let Some(o) = find_field(&mut tx, org_id, &n).await?
                && o.id != f.id
            {
                return Err(ForgeError::already_exists(format!("issue field {n:?}")));
            }
            f.name = n;
        }
        if let Some(d) = description {
            f.description = d.trim().to_string();
        }
        let mut removed_options: Vec<String> = Vec::new();
        if let Some(o) = options {
            if kind_of(&f) != IssueFieldKind::SingleSelect {
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
            let new = assign_option_ids(&tx, &f.options, o)?;
            removed_options = f
                .options
                .iter()
                .filter(|e| !new.iter().any(|n| n.id == e.id))
                .map(|e| e.id.clone())
                .collect();
            f.options = new;
        }
        let mut removed_iterations: Vec<String> = Vec::new();
        if let Some(i) = iterations {
            if kind_of(&f) != IssueFieldKind::Iteration {
                return Err(ForgeError::invalid(
                    "iterations_not_allowed",
                    "iterations belong to iteration fields",
                ));
            }
            let new = assign_iteration_ids(&tx, &f.iterations, i)?;
            removed_iterations = f
                .iterations
                .iter()
                .filter(|e| !new.iter().any(|n| n.id == e.id))
                .map(|e| e.id.clone())
                .collect();
            f.iterations = new;
        }
        if let Some(t) = issue_type_ids {
            let mut types = Vec::new();
            for x in t {
                types.push(resolve_type(&mut tx, org_id, x, false).await?);
            }
            f.issue_type_ids = types;
        }
        f.version = row.version + 1;
        let v = tx
            .update(
                &FIELDS,
                &Row::new(org_id, id)
                    .k1(lc(&f.name))
                    .version(row.version)
                    .msg(&f),
            )
            .await?;
        f.version = v;
        if !removed_options.is_empty() || !removed_iterations.is_empty() {
            let fid = f.id.clone();
            purge_values(&mut tx, org_id, |v| {
                v.field_id == fid
                    && match &v.value {
                        Some(Value::OptionId(o)) => removed_options.contains(o),
                        Some(Value::IterationId(i)) => removed_iterations.contains(i),
                        _ => false,
                    }
            })
            .await?;
        }
        record(&mut tx, field_change("issue_field.update", org_id, &f)).await?;
        tx.commit().await?;
        Ok(f)
    }

    /// Deletes a field and every value set for it.
    pub async fn delete_issue_field(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (f, _) = tx
            .get_msg::<IssueField>(&FIELDS, org_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("issue field {id}")))?;
        purge_values(&mut tx, org_id, |v| v.field_id == id).await?;
        tx.delete(&FIELDS, org_id, id).await?;
        record(&mut tx, field_change("issue_field.delete", org_id, &f)).await?;
        tx.commit().await
    }

    /// An org's fields by name.
    pub async fn list_issue_fields(
        &self,
        cx: &Cx,
        org_id: &str,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<IssueField>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut all: Vec<IssueField> =
            scan_all_msgs::<IssueField>(&mut tx, &FIELDS, Scan::scope(org_id, 1))
                .await?
                .into_iter()
                .map(|(mut f, r)| {
                    f.version = r.version;
                    f
                })
                .collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        let rest: Vec<IssueField> = all
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

    /// Sets field values on an issue.
    pub async fn set_issue_field_values(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        values: &[IssueFieldValue],
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let old = rec.clone();
        let merged = validate_values(
            &mut tx,
            &repo.org_id,
            &rec.issue_type_id,
            &rec.field_values,
            values,
        )
        .await?;
        let fields =
            scan_all_msgs::<IssueField>(&mut tx, &FIELDS, Scan::scope(repo.org_id.clone(), 1))
                .await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let mut changed = Vec::new();
        for nv in &merged {
            let before = rec.field_values.iter().find(|e| e.field_id == nv.field_id);
            if before == Some(nv) {
                continue;
            }
            let Some((f, _)) = fields.iter().find(|(f, _)| f.id == nv.field_id) else {
                continue;
            };
            let prev = before.map(|b| display_value(f, b)).unwrap_or_default();
            let cur = display_value(f, nv);
            let name = f.name.clone();
            add_timeline(&mut tx, &rec.id, Tk::FieldChanged, &actor.principal, |e| {
                e.field_name = name.clone();
                e.previous_value = prev.clone();
                e.current_value = cur.clone();
            })
            .await?;
            changed.push(serde_json::json!({"field": f.name, "from": prev, "to": cur}));
        }
        if changed.is_empty() {
            return render(&mut tx, &mut cache, &rec, row.version).await;
        }
        rec.field_values = merged;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            Change {
                kind: "issue.field_changed",
                action: "issue.set_field_values",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id: &rec.id,
                payload: serde_json::json!({
                    "issue": serde_json::from_str::<serde_json::Value>(&pjson!(&issue)).unwrap_or_default(),
                    "changes": changed,
                })
                .to_string(),
                after: serde_json::json!({"number": rec.number, "changes": changed}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Clears one field value.
    pub async fn clear_issue_field_value(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        field: &str,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let f = find_field(&mut tx, &repo.org_id, field)
            .await?
            .ok_or_else(|| {
                ForgeError::invalid("unknown_field", format!("no issue field {field:?}"))
            })?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let Some(prev) = rec
            .field_values
            .iter()
            .find(|v| v.field_id == f.id)
            .cloned()
        else {
            return render(&mut tx, &mut cache, &rec, row.version).await;
        };
        let old = rec.clone();
        rec.field_values.retain(|v| v.field_id != f.id);
        let (name, prev_display) = (f.name.clone(), display_value(&f, &prev));
        add_timeline(&mut tx, &rec.id, Tk::FieldChanged, &actor.principal, |e| {
            e.field_name = name;
            e.previous_value = prev_display;
        })
        .await?;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            Change {
                kind: "issue.field_changed",
                action: "issue.clear_field_value",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id: &rec.id,
                payload: pjson!(&issue),
                after: serde_json::json!({"number": rec.number, "field": f.name, "cleared": true})
                    .to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }
}

#[cfg(test)]
mod tests {
    use super::iso_date;

    #[test]
    fn dates_render_in_utc() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(951_782_400_000), "2000-02-29");
        assert_eq!(iso_date(1_700_000_000_000), "2023-11-14");
    }
}
