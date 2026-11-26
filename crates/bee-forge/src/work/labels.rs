//! Labels and milestones.

use bee_proto::v1::{
    Issue, IssueTimelineEventKind as Tk, Label, Milestone, MilestoneState, SubjectKind,
};

use super::issues::{Cache, add_timeline, load_issue, render, save_issue};
use super::model::IssueRec;
use super::*;
use crate::ids::{ts, ts_ms};

/// The labels every repo starts with (GitHub's default set).
pub const DEFAULT_LABELS: &[(&str, &str, &str)] = &[
    ("bug", "d73a4a", "Something isn't working"),
    (
        "documentation",
        "0075ca",
        "Improvements or additions to documentation",
    ),
    (
        "duplicate",
        "cfd3d7",
        "This issue or pull request already exists",
    ),
    ("enhancement", "a2eeef", "New feature or request"),
    ("good first issue", "7057ff", "Good for newcomers"),
    ("help wanted", "008672", "Extra attention is needed"),
    ("invalid", "e4e669", "This doesn't seem right"),
    ("question", "d876e3", "Further information is requested"),
    ("wontfix", "ffffff", "This will not be worked on"),
];

const DEFAULT_COLOR: &str = "ededed";
const NAME_MAX: usize = 50;
const DESCRIPTION_MAX: usize = 100;

fn label_audit(l: &Label) -> String {
    serde_json::json!({"label": l.name, "color": l.color}).to_string()
}

/// Finds a label by id or case-insensitive name.
pub(crate) async fn find_label(
    tx: &mut Tx,
    repo_id: &str,
    name_or_id: &str,
) -> Result<Option<Label>> {
    if let Some((l, _)) = tx.get_msg::<Label>(&LABELS, repo_id, name_or_id).await? {
        return Ok(Some(l));
    }
    let rows = tx
        .scan_msgs::<Label>(
            &LABELS,
            &Scan::scope(repo_id, 1).key(Key::K1(lc(name_or_id))),
        )
        .await?;
    Ok(rows.into_iter().next().map(|(l, _)| l))
}

/// Resolves label names to ids, dropping repeats; unknown names are `NOT_FOUND`.
pub(crate) async fn resolve_labels(
    tx: &mut Tx,
    repo_id: &str,
    names: &[String],
    _create_missing: bool,
) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let l = find_label(tx, repo_id, n)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("label {n:?}")))?;
        if !out.contains(&l.id) {
            out.push(l.id);
        }
    }
    Ok(out)
}

/// Finds or silently creates a label (imports); returns its id.
pub(crate) async fn resolve_labels_with_color(
    tx: &mut Tx,
    repo_id: &str,
    name: &str,
    color: &str,
) -> Result<String> {
    if let Some(l) = find_label(tx, repo_id, name).await? {
        return Ok(l.id);
    }
    let name = valid_name("label", name, NAME_MAX)?;
    let id = tx.new_id()?;
    let l = Label {
        id: id.clone(),
        name: name.clone(),
        color: valid_color(color, DEFAULT_COLOR)?,
        description: String::new(),
        is_default: false,
    };
    tx.insert(
        &LABELS,
        &Row::new(repo_id, id.clone()).k1(lc(&name)).msg(&l),
    )
    .await?;
    Ok(id)
}

/// Creates a label and records it.
pub(crate) async fn create_label_in_tx(
    tx: &mut Tx,
    repo: &RepoInfo,
    _actor: &bee_proto::v1::Principal,
    name: &str,
    color: &str,
    description: &str,
) -> Result<Label> {
    let name = valid_name("name", name, NAME_MAX)?;
    if description.chars().count() > DESCRIPTION_MAX {
        return Err(ForgeError::invalid(
            "description_too_long",
            format!("label descriptions are at most {DESCRIPTION_MAX} characters"),
        ));
    }
    if find_label(tx, &repo.id, &name).await?.is_some() {
        return Err(ForgeError::already_exists(format!("label {name:?}")));
    }
    let id = tx.new_id()?;
    let label = Label {
        id: id.clone(),
        name: name.clone(),
        color: valid_color(color, DEFAULT_COLOR)?,
        description: description.trim().to_string(),
        is_default: false,
    };
    tx.insert(
        &LABELS,
        &Row::new(repo.id.clone(), id).k1(lc(&name)).msg(&label),
    )
    .await?;
    record(
        tx,
        Change {
            kind: "label.created",
            action: "label.create",
            org_id: &repo.org_id,
            repo_id: &repo.id,
            subject_kind: SubjectKind::Repo,
            subject_id: &repo.id,
            payload: pjson!(&label),
            after: label_audit(&label),
        },
    )
    .await?;
    Ok(label)
}

fn adjust_counts(m: &mut Milestone, open: i64, closed: i64) {
    m.open_items = (i64::from(m.open_items) + open).max(0) as u32;
    m.closed_items = (i64::from(m.closed_items) + closed).max(0) as u32;
}

async fn milestone_row(
    tx: &mut Tx,
    repo_id: &str,
    number: u64,
) -> Result<Option<(Milestone, Row)>> {
    let rows = tx
        .scan_msgs::<Milestone>(
            &MILESTONES,
            &Scan::scope(repo_id, 1).key(Key::K2(pad(number))),
        )
        .await?;
    Ok(rows.into_iter().next())
}

async fn write_milestone(tx: &mut Tx, repo_id: &str, m: &Milestone, row: &Row) -> Result<()> {
    let new = Row::new(repo_id, row.id.clone())
        .k1(lc(&m.title))
        .k2(pad(m.number))
        .version(row.version)
        .msg(m);
    tx.update(&MILESTONES, &new).await?;
    Ok(())
}

/// Requires that milestone `number` exists in the repo.
pub(crate) async fn require_milestone(
    tx: &mut Tx,
    repo_id: &str,
    number: u64,
) -> Result<Milestone> {
    milestone_row(tx, repo_id, number)
        .await?
        .map(|(m, _)| m)
        .ok_or_else(|| ForgeError::not_found(format!("milestone {number}")))
}

/// A milestone's title (empty when gone).
pub(crate) async fn milestone_title(tx: &mut Tx, repo_id: &str, number: u64) -> Result<String> {
    Ok(milestone_row(tx, repo_id, number)
        .await?
        .map(|(m, _)| m.title)
        .unwrap_or_default())
}

/// A milestone by title.
pub(crate) async fn find_milestone_by_title(
    tx: &mut Tx,
    repo_id: &str,
    title: &str,
) -> Result<Option<Milestone>> {
    let rows = tx
        .scan_msgs::<Milestone>(
            &MILESTONES,
            &Scan::scope(repo_id, 1).key(Key::K1(lc(title))),
        )
        .await?;
    Ok(rows.into_iter().next().map(|(m, _)| m))
}

/// Finds or silently creates a milestone by title (imports); returns its number.
pub(crate) async fn ensure_milestone(tx: &mut Tx, repo_id: &str, title: &str) -> Result<u64> {
    if let Some(m) = find_milestone_by_title(tx, repo_id, title).await? {
        return Ok(m.number);
    }
    let title = valid_name("title", title, 256)?;
    let number = next_counter(tx, repo_id, "milestones").await?;
    let id = tx.new_id()?;
    let m = Milestone {
        id: id.clone(),
        number,
        title: title.clone(),
        state: MilestoneState::Open as i32,
        created_at: Some(ts(tx.now_ms())),
        ..Default::default()
    };
    tx.insert(
        &MILESTONES,
        &Row::new(repo_id, id).k1(lc(&title)).k2(pad(number)).msg(&m),
    )
    .await?;
    Ok(number)
}

/// Keeps milestone open/closed counters right when an issue's milestone or state changes.
pub(crate) async fn track_milestone(
    tx: &mut Tx,
    old: Option<&IssueRec>,
    new: &IssueRec,
) -> Result<()> {
    use bee_proto::v1::IssueState;
    let bucket = |r: &IssueRec| {
        (r.milestone_number > 0 && r.state != IssueState::Unspecified as i32).then(|| {
            (
                r.repo_id.clone(),
                r.milestone_number,
                r.state != IssueState::Closed as i32,
            )
        })
    };
    let (o, n) = (old.and_then(bucket), bucket(new));
    if o == n {
        return Ok(());
    }
    for (b, sign) in [(o, -1i64), (n, 1i64)] {
        if let Some((repo, num, open)) = b
            && let Some((mut m, row)) = milestone_row(tx, &repo, num).await?
        {
            if open {
                adjust_counts(&mut m, sign, 0);
            } else {
                adjust_counts(&mut m, 0, sign);
            }
            write_milestone(tx, &repo, &m, &row).await?;
        }
    }
    Ok(())
}

async fn all_issues_of(tx: &mut Tx, repo_id: &str) -> Result<Vec<(IssueRec, Row)>> {
    scan_all_msgs::<IssueRec>(tx, &ISSUES, Scan::scope(repo_id, 1)).await
}

impl Work {
    /// Creates the default label set once per repo (idempotent). The service calls it before
    /// any label operation; the server may also call it when a repo is created.
    pub async fn seed_default_labels(&self, cx: &Cx, repo: &RepoInfo) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if tx
            .get(&COUNTERS, &repo.id, "labels_seeded")
            .await?
            .is_some()
        {
            return Ok(());
        }
        for (name, color, desc) in DEFAULT_LABELS {
            if find_label(&mut tx, &repo.id, name).await?.is_some() {
                continue;
            }
            let id = tx.new_id()?;
            let l = Label {
                id: id.clone(),
                name: (*name).into(),
                color: (*color).into(),
                description: (*desc).into(),
                is_default: true,
            };
            tx.insert(&LABELS, &Row::new(repo.id.clone(), id).k1(lc(name)).msg(&l))
                .await?;
        }
        tx.add_n1(&COUNTERS, &repo.id, "labels_seeded", 1).await?;
        tx.commit().await
    }

    /// Creates a repo label.
    pub async fn create_label(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        name: &str,
        color: &str,
        description: &str,
    ) -> Result<Label> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let l =
            create_label_in_tx(&mut tx, repo, &actor.principal, name, color, description).await?;
        tx.commit().await?;
        Ok(l)
    }

    /// One label by name or id.
    pub async fn get_label(&self, cx: &Cx, repo: &RepoInfo, name: &str) -> Result<Label> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        find_label(&mut tx, &repo.id, name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("label {name:?}")))
    }

    /// Renames or recolors a label; issues carrying it keep it.
    pub async fn update_label(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        name: &str,
        new_name: Option<String>,
        color: Option<String>,
        description: Option<String>,
    ) -> Result<Label> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut l = find_label(&mut tx, &repo.id, name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("label {name:?}")))?;
        let row = tx
            .get_for_update(&LABELS, &repo.id, &l.id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("label {name:?}")))?;
        if let Some(n) = new_name {
            let n = valid_name("new_name", &n, NAME_MAX)?;
            if let Some(other) = find_label(&mut tx, &repo.id, &n).await?
                && other.id != l.id
            {
                return Err(ForgeError::already_exists(format!("label {n:?}")));
            }
            l.name = n;
        }
        if let Some(c) = color {
            l.color = valid_color(&c, DEFAULT_COLOR)?;
        }
        if let Some(d) = description {
            if d.chars().count() > DESCRIPTION_MAX {
                return Err(ForgeError::invalid(
                    "description_too_long",
                    format!("label descriptions are at most {DESCRIPTION_MAX} characters"),
                ));
            }
            l.description = d.trim().to_string();
        }
        let new = Row::new(repo.id.clone(), l.id.clone())
            .k1(lc(&l.name))
            .version(row.version)
            .msg(&l);
        tx.update(&LABELS, &new).await?;
        // Items snapshot label names.
        for (rec, _) in all_issues_of(&mut tx, &repo.id).await? {
            if rec.label_ids.contains(&l.id) {
                super::items::refresh_issue(&mut tx, &rec).await?;
            }
        }
        record(
            &mut tx,
            Change {
                kind: "label.edited",
                action: "label.update",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Repo,
                subject_id: &repo.id,
                payload: pjson!(&l),
                after: label_audit(&l),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(l)
    }

    /// Deletes a label and removes it from every issue.
    pub async fn delete_label(&self, cx: &Cx, repo: &RepoInfo, name: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let l = find_label(&mut tx, &repo.id, name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("label {name:?}")))?;
        for (mut rec, r) in all_issues_of(&mut tx, &repo.id).await? {
            if rec.label_ids.contains(&l.id) {
                let old = rec.clone();
                rec.label_ids.retain(|x| *x != l.id);
                save_issue(&mut tx, Some(&old), &mut rec, r.version).await?;
            }
        }
        tx.delete(&LABELS, &repo.id, &l.id).await?;
        record(
            &mut tx,
            Change {
                kind: "label.deleted",
                action: "label.delete",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Repo,
                subject_id: &repo.id,
                payload: pjson!(&l),
                after: label_audit(&l),
            },
        )
        .await?;
        tx.commit().await
    }

    /// The repo's labels by name.
    pub async fn list_labels(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        query: &str,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<Label>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let q = lc(query);
        let mut all: Vec<Label> =
            scan_all_msgs::<Label>(&mut tx, &LABELS, Scan::scope(repo.id.clone(), 1))
                .await?
                .into_iter()
                .map(|(l, _)| l)
                .filter(|l| q.is_empty() || l.name.to_lowercase().contains(&q))
                .collect();
        all.sort_by_key(|l| (lc(&l.name), l.id.clone()));
        let start = if after.is_empty() {
            0
        } else {
            all.iter()
                .position(|l| (lc(&l.name), l.id.clone()) > split_token(after))
                .unwrap_or(all.len())
        };
        let rest: Vec<Label> = all.into_iter().skip(start).collect();
        let next = if rest.len() > limit {
            format!("{}\u{1}{}", lc(&rest[limit - 1].name), rest[limit - 1].id)
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }

    async fn relabel(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        expected_version: u64,
        edit: LabelEdit<'_>,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        check_version(expected_version, row.version)?;
        let old = rec.clone();
        let target: Vec<String> = match edit {
            LabelEdit::Add {
                names,
                create_missing,
            } => {
                let mut ids = rec.label_ids.clone();
                for n in names {
                    let id = match find_label(&mut tx, &repo.id, n).await? {
                        Some(l) => l.id,
                        None if create_missing => {
                            create_label_in_tx(&mut tx, repo, &actor.principal, n, "", "")
                                .await?
                                .id
                        }
                        None => return Err(ForgeError::not_found(format!("label {n:?}"))),
                    };
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                ids
            }
            LabelEdit::Remove { names } => {
                let mut ids = rec.label_ids.clone();
                for n in names {
                    if let Some(l) = find_label(&mut tx, &repo.id, n).await? {
                        ids.retain(|x| *x != l.id);
                    }
                }
                ids
            }
            LabelEdit::Set { names } => resolve_labels(&mut tx, &repo.id, names, false).await?,
        };
        if target.len() > limits::LABELS {
            return Err(exhausted("labels", limits::LABELS, target.len()));
        }
        let me = actor.principal.clone();
        let mut changes = Vec::new();
        for id in target.iter().filter(|i| !rec.label_ids.contains(i)) {
            let l = tx
                .get_msg::<Label>(&LABELS, &repo.id, id)
                .await?
                .map(|(l, _)| l);
            add_timeline(&mut tx, &rec.id, Tk::Labeled, &me, |e| e.label = l.clone()).await?;
            changes.push(("added", l));
        }
        for id in rec.label_ids.iter().filter(|i| !target.contains(i)) {
            let l = tx
                .get_msg::<Label>(&LABELS, &repo.id, id)
                .await?
                .map(|(l, _)| l);
            add_timeline(&mut tx, &rec.id, Tk::Unlabeled, &me, |e| {
                e.label = l.clone()
            })
            .await?;
            changes.push(("removed", l));
        }
        if changes.is_empty() {
            let mut cache = Cache::new(repo, &cx.actor_id);
            return render(&mut tx, &mut cache, &rec, row.version).await;
        }
        rec.label_ids = target;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        let delta: Vec<serde_json::Value> = changes
            .iter()
            .map(|(a, l)| serde_json::json!({"change": a, "label": l.as_ref().map(|l| l.name.clone())}))
            .collect();
        record(
            &mut tx,
            Change {
                kind: "issue.labeled",
                action: "issue.label",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id: &rec.id,
                payload: serde_json::json!({
                    "issue": serde_json::from_str::<serde_json::Value>(&pjson!(&issue)).unwrap_or_default(),
                    "changes": delta,
                })
                .to_string(),
                after: serde_json::json!({"number": rec.number, "changes": delta}).to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// Adds labels to an issue.
    pub async fn add_issue_labels(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        names: &[String],
        create_missing: bool,
    ) -> Result<Issue> {
        self.relabel(
            cx,
            repo,
            actor,
            number,
            0,
            LabelEdit::Add {
                names,
                create_missing,
            },
        )
        .await
    }

    /// Removes labels from an issue.
    pub async fn remove_issue_labels(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        names: &[String],
    ) -> Result<Issue> {
        self.relabel(cx, repo, actor, number, 0, LabelEdit::Remove { names })
            .await
    }

    /// Replaces an issue's label set.
    pub async fn set_issue_labels(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        names: &[String],
        expected_version: u64,
    ) -> Result<Issue> {
        self.relabel(
            cx,
            repo,
            actor,
            number,
            expected_version,
            LabelEdit::Set { names },
        )
        .await
    }

    // ---- milestones ----

    /// Creates a milestone.
    pub async fn create_milestone(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        title: &str,
        description: &str,
        due_ms: Option<u64>,
    ) -> Result<Milestone> {
        let title = valid_name("title", title, 256)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if find_milestone_by_title(&mut tx, &repo.id, &title)
            .await?
            .is_some()
        {
            return Err(ForgeError::already_exists(format!("milestone {title:?}")));
        }
        let number = next_counter(&mut tx, &repo.id, "milestones").await?;
        let id = tx.new_id()?;
        let m = Milestone {
            id: id.clone(),
            number,
            title: title.clone(),
            description: description.to_string(),
            state: MilestoneState::Open as i32,
            due_on: due_ms.map(ts),
            created_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        tx.insert(
            &MILESTONES,
            &Row::new(repo.id.clone(), id)
                .k1(lc(&title))
                .k2(pad(number))
                .msg(&m),
        )
        .await?;
        record(
            &mut tx,
            milestone_change("milestone.created", "milestone.create", repo, &m),
        )
        .await?;
        tx.commit().await?;
        Ok(m)
    }

    /// One milestone by number.
    pub async fn get_milestone(&self, cx: &Cx, repo: &RepoInfo, number: u64) -> Result<Milestone> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        require_milestone(&mut tx, &repo.id, number).await
    }

    /// Edits a milestone.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_milestone(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        number: u64,
        title: Option<String>,
        description: Option<String>,
        due_ms: Option<u64>,
        clear_due: bool,
        state: MilestoneState,
    ) -> Result<Milestone> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut m, row) = milestone_row(&mut tx, &repo.id, number)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("milestone {number}")))?;
        let before_state = m.state;
        if let Some(t) = title {
            let t = valid_name("title", &t, 256)?;
            if let Some(o) = find_milestone_by_title(&mut tx, &repo.id, &t).await?
                && o.number != m.number
            {
                return Err(ForgeError::already_exists(format!("milestone {t:?}")));
            }
            m.title = t;
        }
        if let Some(d) = description {
            m.description = d;
        }
        if clear_due {
            m.due_on = None;
        } else if let Some(d) = due_ms {
            m.due_on = Some(ts(d));
        }
        match state {
            MilestoneState::Open if m.state != MilestoneState::Open as i32 => {
                m.state = MilestoneState::Open as i32;
                m.closed_at = None;
            }
            MilestoneState::Closed if m.state != MilestoneState::Closed as i32 => {
                m.state = MilestoneState::Closed as i32;
                m.closed_at = Some(ts(tx.now_ms()));
            }
            _ => {}
        }
        write_milestone(&mut tx, &repo.id, &m, &row).await?;
        // Items snapshot milestone titles.
        for (rec, _) in all_issues_of(&mut tx, &repo.id).await? {
            if rec.milestone_number == m.number {
                super::items::refresh_issue(&mut tx, &rec).await?;
            }
        }
        let (kind, action) = if before_state != m.state {
            if m.state == MilestoneState::Closed as i32 {
                ("milestone.closed", "milestone.close")
            } else {
                ("milestone.opened", "milestone.reopen")
            }
        } else {
            ("milestone.edited", "milestone.update")
        };
        record(&mut tx, milestone_change(kind, action, repo, &m)).await?;
        tx.commit().await?;
        Ok(m)
    }

    /// Deletes a milestone and clears it from every issue.
    pub async fn delete_milestone(&self, cx: &Cx, repo: &RepoInfo, number: u64) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (m, row) = milestone_row(&mut tx, &repo.id, number)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("milestone {number}")))?;
        for (mut rec, r) in all_issues_of(&mut tx, &repo.id).await? {
            if rec.milestone_number == number {
                rec.milestone_number = 0;
                // The milestone is going away, so its counters need no adjusting.
                let old = rec.clone();
                let mut keep = old.clone();
                keep.milestone_number = 0;
                save_issue(&mut tx, Some(&keep), &mut rec, r.version).await?;
            }
        }
        tx.delete(&MILESTONES, &repo.id, &row.id).await?;
        record(
            &mut tx,
            milestone_change("milestone.deleted", "milestone.delete", repo, &m),
        )
        .await?;
        tx.commit().await
    }

    /// A repo's milestones by number.
    pub async fn list_milestones(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        state: Option<MilestoneState>,
        limit: usize,
        after: &str,
    ) -> Result<(Vec<Milestone>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut all: Vec<Milestone> =
            scan_all_msgs::<Milestone>(&mut tx, &MILESTONES, Scan::scope(repo.id.clone(), 1))
                .await?
                .into_iter()
                .map(|(m, _)| m)
                .filter(|m| state.is_none_or(|s| m.state == s as i32))
                .collect();
        all.sort_by_key(|m| m.number);
        let after_n: u64 = after.parse().unwrap_or(0);
        let rest: Vec<Milestone> = all.into_iter().filter(|m| m.number > after_n).collect();
        let next = if rest.len() > limit {
            rest[limit - 1].number.to_string()
        } else {
            String::new()
        };
        Ok((rest.into_iter().take(limit).collect(), next))
    }

    /// Sets or clears (0) an issue's milestone.
    pub async fn set_issue_milestone(
        &self,
        cx: &Cx,
        repo: &RepoInfo,
        actor: &Actor,
        number: u64,
        milestone_number: u64,
    ) -> Result<Issue> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = load_issue(&mut tx, &repo.id, number, true).await?;
        let mut cache = Cache::new(repo, &cx.actor_id);
        if rec.milestone_number == milestone_number {
            return render(&mut tx, &mut cache, &rec, row.version).await;
        }
        let old = rec.clone();
        let me = actor.principal.clone();
        if rec.milestone_number > 0 {
            let t = milestone_title(&mut tx, &repo.id, rec.milestone_number).await?;
            add_timeline(&mut tx, &rec.id, Tk::Demilestoned, &me, |e| {
                e.milestone_title = t
            })
            .await?;
        }
        if milestone_number > 0 {
            let m = require_milestone(&mut tx, &repo.id, milestone_number).await?;
            add_timeline(&mut tx, &rec.id, Tk::Milestoned, &me, |e| {
                e.milestone_title = m.title
            })
            .await?;
        }
        rec.milestone_number = milestone_number;
        let v = save_issue(&mut tx, Some(&old), &mut rec, row.version).await?;
        cache = Cache::new(repo, &cx.actor_id);
        let issue = render(&mut tx, &mut cache, &rec, v).await?;
        record(
            &mut tx,
            Change {
                kind: "issue.milestoned",
                action: "issue.milestone",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                subject_kind: SubjectKind::Issue,
                subject_id: &rec.id,
                payload: pjson!(&issue),
                after: serde_json::json!({"number": rec.number, "milestone": milestone_number})
                    .to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(issue)
    }
}

enum LabelEdit<'a> {
    Add {
        names: &'a [String],
        create_missing: bool,
    },
    Remove {
        names: &'a [String],
    },
    Set {
        names: &'a [String],
    },
}

fn split_token(t: &str) -> (String, String) {
    match t.split_once('\u{1}') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (t.to_string(), String::new()),
    }
}

fn milestone_change<'a>(
    kind: &'a str,
    action: &'a str,
    repo: &'a RepoInfo,
    m: &Milestone,
) -> Change<'a> {
    Change {
        kind,
        action,
        org_id: &repo.org_id,
        repo_id: &repo.id,
        subject_kind: SubjectKind::Repo,
        subject_id: &repo.id,
        payload: pjson!(m),
        after: serde_json::json!({
            "milestone": m.number, "title": m.title,
            "due_ms": m.due_on.as_ref().map(ts_ms).unwrap_or(0),
        })
        .to_string(),
    }
}
