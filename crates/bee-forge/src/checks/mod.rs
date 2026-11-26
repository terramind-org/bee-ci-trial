//! Check runs, suites, annotations, rerequests and commit statuses (SPEC §11.6, PRODUCT
//! §4.14).
//!
//! - A **check run** belongs to one changeset and has a name; reporting the same name again
//!   creates a new run, and "latest per name" is what required-check evaluation reads
//!   ([`Checks::check_results`]).
//! - **Annotations** are appended on every report or update (at most
//!   [`MAX_ANNOTATIONS_PER_RUN`] per run) and stored apart from the run, so a run stays small.
//! - A **suite** groups runs of one app for one changeset; its status and run count are
//!   aggregated from its runs when read.
//! - **Rerequest** creates a fresh queued run (or one per latest run of a suite) and emits
//!   `check_run.rerequested` / `check_suite.rerequested` for the reporter to act on.
//! - **Commit statuses** (GitHub's legacy statuses API) are stored as statuses and mirrored
//!   onto a check run named after the context with `external_id = "status:<context>"`, so
//!   policy sees one model.
//! - `post_land` runs are reported on landed changesets; [`Checks::failing_post_land`] is the
//!   input of post-land bisect (the `ci` module).
//!
//! Every write emits `check.updated` (or the specific kind) and an audit entry.

use std::collections::BTreeMap;

use bee_proto::v1::{
    CheckAction, CheckAnnotation, CheckOutput, CheckRun, CheckStatus, CheckSuite, CombinedStatus,
    CommitStatus, CommitStatusState, Event, ObjectId, SubjectKind, event,
};

use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts, ts_ms};
use crate::module::{EventKind, ForgeModule};
use crate::store::{
    Cursor, Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef, Tx,
};

/// Most annotations stored per run.
pub const MAX_ANNOTATIONS_PER_RUN: u32 = 10_000;
/// Most annotations per report or update call.
pub const MAX_ANNOTATIONS_PER_CALL: usize = 1000;
/// Most runs returned by one list.
pub const MAX_LIST: usize = 1000;

/// Runs, scoped by repo; `k1` changeset hex, `k2` suite id, `n1` creation (ms).
pub static RUNS: TableDef = TableDef {
    name: "forge_check_runs",
    surface: Surface::CHECKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeK2, Index::ScopeN1],
};

/// Annotations, scoped by run id, ordered by index.
pub static ANNOTATIONS: TableDef = TableDef {
    name: "forge_check_annotations",
    surface: Surface::CHECKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Suites, scoped by repo; `k1` changeset hex, `k2` app id.
pub static SUITES: TableDef = TableDef {
    name: "forge_check_suites",
    surface: Surface::CHECKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Commit statuses, scoped by repo; `k1` changeset hex, `k2` context, `n1` creation.
pub static STATUSES: TableDef = TableDef {
    name: "forge_commit_statuses",
    surface: Surface::CHECKS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

static M1: Migration = Migration {
    module: "checks",
    version: 1,
    name: "create_checks",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&RUNS),
        MigrationOp::CreateTable(&ANNOTATIONS),
        MigrationOp::CreateTable(&SUITES),
        MigrationOp::CreateTable(&STATUSES),
    ],
};

/// What the checks module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "checks",
    surfaces: &[Surface::CHECKS],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::gh(
            "check.updated",
            "A check run was reported or changed",
            "check_run",
            "completed",
        ),
        EventKind::gh(
            "check_run.rerequested",
            "A check run was rerequested",
            "check_run",
            "rerequested",
        ),
        EventKind::gh(
            "check_suite.created",
            "A check suite was created",
            "check_suite",
            "requested",
        ),
        EventKind::gh(
            "check_suite.rerequested",
            "A check suite was rerequested",
            "check_suite",
            "rerequested",
        ),
        EventKind::gh("status.created", "A commit status was set", "status", ""),
    ],
    audit_prefixes: &["check.", "check_suite.", "status."],
};

fn cs_hex(c: &Option<ObjectId>) -> Result<String> {
    match c {
        Some(o) if !o.raw.is_empty() && o.raw.len() <= 64 => Ok(hex(&o.raw)),
        Some(o) if o.raw.len() > 64 => {
            Err(ForgeError::invalid("invalid_id", "changeset id too long"))
        }
        _ => Err(ForgeError::missing("changeset")),
    }
}

fn terminal(s: CheckStatus) -> bool {
    !matches!(
        s,
        CheckStatus::Unspecified | CheckStatus::Queued | CheckStatus::Running
    )
}

/// Maps a check status onto the policy evaluator's status.
pub fn policy_status(s: CheckStatus) -> bee_policy::CheckStatus {
    use bee_policy::CheckStatus as P;
    match s {
        CheckStatus::Passed => P::Success,
        CheckStatus::Skipped => P::Skipped,
        CheckStatus::Neutral => P::Neutral,
        CheckStatus::Failed
        | CheckStatus::Cancelled
        | CheckStatus::TimedOut
        | CheckStatus::ActionRequired
        | CheckStatus::Stale => P::Failure,
        CheckStatus::Unspecified | CheckStatus::Queued | CheckStatus::Running => P::Pending,
    }
}

fn status_of(r: &CheckRun) -> CheckStatus {
    CheckStatus::try_from(r.status).unwrap_or(CheckStatus::Unspecified)
}

/// A new check run.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Changeset checked.
    pub changeset: Option<ObjectId>,
    /// Check name.
    pub name: String,
    /// Status (unspecified = queued).
    pub status: CheckStatus,
    /// Details URL.
    pub url: String,
    /// One-line summary.
    pub summary: String,
    /// Post-land verification check.
    pub post_land: bool,
    /// Reporter's id.
    pub external_id: String,
    /// Rich output (annotations are moved to the annotation table).
    pub output: Option<CheckOutput>,
    /// Suite.
    pub suite_id: String,
    /// Buttons.
    pub actions: Vec<CheckAction>,
    /// Start time (ms; 0 = now when running).
    pub started_at_ms: u64,
    /// Finish time (ms; 0 = now when terminal).
    pub finished_at_ms: u64,
    /// Workflow job that produced it.
    pub workflow_job_id: String,
}

/// A partial update of a run.
#[derive(Clone, Debug, Default)]
pub struct RunPatch {
    /// New status.
    pub status: Option<CheckStatus>,
    /// New URL.
    pub url: Option<String>,
    /// New summary.
    pub summary: Option<String>,
    /// Output: annotations are appended, other set fields replace.
    pub output: Option<CheckOutput>,
    /// Replaces the actions when non-empty.
    pub actions: Vec<CheckAction>,
    /// Finish time (ms).
    pub finished_at_ms: u64,
}

/// Run filters.
#[derive(Clone, Debug, Default)]
pub struct RunFilter {
    /// Changeset (hex), or all.
    pub changeset_hex: Option<String>,
    /// Name.
    pub name: String,
    /// Suite.
    pub suite_id: String,
    /// Only the newest run per name.
    pub latest_only: bool,
}

/// Checks over the store.
pub struct Checks {
    db: Db,
}

impl Checks {
    /// Checks over `db`.
    pub fn new(db: Db) -> Checks {
        Checks { db }
    }

    async fn emit(tx: &mut Tx, kind: &str, org_id: &str, run: &CheckRun) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: run.repo_id.clone(),
            org_id: org_id.into(),
            body: Some(event::Body::Check(run.clone())),
            ..Default::default()
        })
        .await
    }

    async fn append_annotations(
        tx: &mut Tx,
        run: &mut CheckRun,
        annotations: Vec<CheckAnnotation>,
    ) -> Result<()> {
        if annotations.len() > MAX_ANNOTATIONS_PER_CALL {
            return Err(ForgeError::too_large(
                "annotations_per_call",
                MAX_ANNOTATIONS_PER_CALL as u64,
                annotations.len() as u64,
            ));
        }
        let total = run.annotations_count as u64 + annotations.len() as u64;
        if total > u64::from(MAX_ANNOTATIONS_PER_RUN) {
            return Err(ForgeError::too_large(
                "annotations_per_run",
                u64::from(MAX_ANNOTATIONS_PER_RUN),
                total,
            ));
        }
        for a in annotations {
            let idx = run.annotations_count;
            tx.insert(
                &ANNOTATIONS,
                &Row::new(run.id.clone(), format!("{idx:010}")).msg(&a),
            )
            .await?;
            run.annotations_count += 1;
        }
        Ok(())
    }

    /// Reports a new run.
    pub async fn report(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        r: Report,
    ) -> Result<CheckRun> {
        let cs = cs_hex(&r.changeset)?;
        if r.name.is_empty() {
            return Err(ForgeError::missing("name"));
        }
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        if !r.suite_id.is_empty() && tx.get(&SUITES, repo_id, &r.suite_id).await?.is_none() {
            return Err(ForgeError::not_found(format!("check suite {}", r.suite_id)));
        }
        let now = tx.now_ms();
        let status = if r.status == CheckStatus::Unspecified {
            CheckStatus::Queued
        } else {
            r.status
        };
        let mut output = r.output.unwrap_or_default();
        let annotations = std::mem::take(&mut output.annotations);
        let started = match (r.started_at_ms, status) {
            (0, CheckStatus::Queued) => None,
            (0, _) => Some(ts(now)),
            (t, _) => Some(ts(t)),
        };
        let finished = match (r.finished_at_ms, terminal(status)) {
            (0, true) => Some(ts(now)),
            (0, false) => None,
            (t, _) => Some(ts(t)),
        };
        let mut run = CheckRun {
            id: tx.new_id()?,
            repo_id: repo_id.to_string(),
            changeset: r.changeset.clone(),
            name: r.name,
            status: status as i32,
            url: r.url,
            summary: r.summary,
            post_land: r.post_land,
            started_at: started,
            finished_at: finished,
            external_id: r.external_id,
            output: Some(output),
            suite_id: r.suite_id.clone(),
            reporter: Some(cx.principal()),
            actions: r.actions,
            annotations_count: 0,
            workflow_job_id: r.workflow_job_id,
        };
        Self::append_annotations(&mut tx, &mut run, annotations).await?;
        let mut row = Row::new(repo_id, run.id.clone())
            .k1(cs)
            .n1(now as i64)
            .msg(&run);
        if !r.suite_id.is_empty() {
            row = row.k2(r.suite_id);
        }
        tx.insert(&RUNS, &row).await?;
        Self::emit(&mut tx, "check.updated", org_id, &run).await?;
        tx.audit(
            AuditDraft::new("check.report", org_id)
                .repo_id(repo_id)
                .target(SubjectKind::CheckRun, &run.id)
                .after(&run_json(&run)),
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    /// One run.
    pub async fn get(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<CheckRun> {
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        match tx.get_msg::<CheckRun>(&RUNS, repo_id, id).await? {
            Some((r, _)) => Ok(r),
            None => Err(ForgeError::not_found(format!("check run {id}"))),
        }
    }

    /// Updates a run.
    pub async fn update(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        id: &str,
        p: RunPatch,
    ) -> Result<CheckRun> {
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let Some((mut run, row)) = tx.get_msg::<CheckRun>(&RUNS, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("check run {id}")));
        };
        let before = run_json(&run);
        let now = tx.now_ms();
        if let Some(s) = p.status {
            run.status = s as i32;
            if s == CheckStatus::Running && run.started_at.is_none() {
                run.started_at = Some(ts(now));
            }
            if terminal(s) && run.finished_at.is_none() && p.finished_at_ms == 0 {
                run.finished_at = Some(ts(now));
            }
            if !terminal(s) {
                run.finished_at = None;
            }
        }
        if p.finished_at_ms != 0 {
            run.finished_at = Some(ts(p.finished_at_ms));
        }
        if let Some(u) = p.url {
            run.url = u;
        }
        if let Some(s) = p.summary {
            run.summary = s;
        }
        if !p.actions.is_empty() {
            run.actions = p.actions;
        }
        if let Some(mut o) = p.output {
            let annotations = std::mem::take(&mut o.annotations);
            let cur = run.output.get_or_insert_with(CheckOutput::default);
            if !o.title.is_empty() {
                cur.title = o.title;
            }
            if !o.summary.is_empty() {
                cur.summary = o.summary;
            }
            if !o.text.is_empty() {
                cur.text = o.text;
            }
            if !o.images.is_empty() {
                cur.images = o.images;
            }
            Self::append_annotations(&mut tx, &mut run, annotations).await?;
        }
        tx.update(&RUNS, &row.clone().msg(&run)).await?;
        Self::emit(&mut tx, "check.updated", org_id, &run).await?;
        tx.audit(
            AuditDraft::new("check.update", org_id)
                .repo_id(repo_id)
                .target(SubjectKind::CheckRun, &run.id)
                .before(&before)
                .after(&run_json(&run)),
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    /// Runs matching the filter, newest first (at most [`MAX_LIST`]).
    pub async fn list(&self, cx: &Cx, repo_id: &str, f: &RunFilter) -> Result<Vec<CheckRun>> {
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let mut q = Scan::scope(repo_id, MAX_LIST).by_n1().desc();
        if let Some(c) = &f.changeset_hex {
            q = q.key(Key::K1(c.clone()));
        } else if !f.suite_id.is_empty() {
            q = q.key(Key::K2(f.suite_id.clone()));
        }
        let rows = tx.scan_msgs::<CheckRun>(&RUNS, &q).await?;
        let mut seen = std::collections::BTreeSet::new();
        Ok(rows
            .into_iter()
            .map(|(r, _)| r)
            .filter(|r| f.name.is_empty() || r.name == f.name)
            .filter(|r| f.suite_id.is_empty() || r.suite_id == f.suite_id)
            .filter(|r| !f.latest_only || seen.insert(r.name.clone()))
            .collect())
    }

    /// A run's annotations, paged by index.
    pub async fn annotations(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<CheckAnnotation>, String)> {
        self.get(cx, repo_id, run_id).await?;
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let cursor = (!after.is_empty()).then(|| Cursor {
            n1: 0,
            scope: run_id.to_string(),
            id: after.to_string(),
        });
        let rows = tx
            .scan_msgs::<CheckAnnotation>(
                &ANNOTATIONS,
                &Scan::scope(run_id, limit + 1).after(cursor),
            )
            .await?;
        let more = rows.len() > limit;
        let next = if more {
            rows.get(limit - 1)
                .map(|(_, r)| r.id.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok((rows.into_iter().take(limit).map(|(a, _)| a).collect(), next))
    }

    async fn requeue(tx: &mut Tx, cx: &Cx, old: &CheckRun) -> Result<CheckRun> {
        let now = tx.now_ms();
        let run = CheckRun {
            id: tx.new_id()?,
            status: CheckStatus::Queued as i32,
            started_at: None,
            finished_at: None,
            output: None,
            annotations_count: 0,
            reporter: Some(cx.principal()),
            ..old.clone()
        };
        let cs = cs_hex(&run.changeset)?;
        let mut row = Row::new(run.repo_id.clone(), run.id.clone())
            .k1(cs)
            .n1(now as i64)
            .msg(&run);
        if !run.suite_id.is_empty() {
            row = row.k2(run.suite_id.clone());
        }
        tx.insert(&RUNS, &row).await?;
        Ok(run)
    }

    /// Queues a fresh run with the same name, changeset and suite, and emits
    /// `check_run.rerequested`.
    pub async fn rerequest_run(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        id: &str,
    ) -> Result<CheckRun> {
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let Some((old, _)) = tx.get_msg::<CheckRun>(&RUNS, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("check run {id}")));
        };
        let run = Self::requeue(&mut tx, cx, &old).await?;
        Self::emit(&mut tx, "check_run.rerequested", org_id, &run).await?;
        tx.audit(
            AuditDraft::new("check.rerequest", org_id)
                .repo_id(repo_id)
                .target(SubjectKind::CheckRun, &run.id)
                .before(&run_json(&old)),
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    fn aggregate(suite: &mut CheckSuite, runs: &[CheckRun]) {
        let mut latest: BTreeMap<&str, &CheckRun> = BTreeMap::new();
        for r in runs {
            latest.entry(r.name.as_str()).or_insert(r);
        }
        suite.runs_count = runs.len() as u32;
        let statuses: Vec<CheckStatus> = latest.values().map(|r| status_of(r)).collect();
        let status = if statuses.is_empty() {
            CheckStatus::Queued
        } else if statuses.iter().any(|s| !terminal(*s)) {
            if statuses.contains(&CheckStatus::Running) {
                CheckStatus::Running
            } else {
                CheckStatus::Queued
            }
        } else if statuses
            .iter()
            .any(|s| policy_status(*s) == bee_policy::CheckStatus::Failure)
        {
            CheckStatus::Failed
        } else {
            CheckStatus::Passed
        };
        suite.status = status as i32;
    }

    async fn suite_runs(tx: &mut Tx, repo_id: &str, suite_id: &str) -> Result<Vec<CheckRun>> {
        Ok(tx
            .scan_msgs::<CheckRun>(
                &RUNS,
                &Scan::scope(repo_id, MAX_LIST)
                    .by_n1()
                    .desc()
                    .key(Key::K2(suite_id.to_string())),
            )
            .await?
            .into_iter()
            .map(|(r, _)| r)
            .collect())
    }

    /// Creates a suite for the calling app (the actor id is the app id).
    pub async fn create_suite(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        changeset: Option<ObjectId>,
        head_ref: &str,
    ) -> Result<CheckSuite> {
        let cs = cs_hex(&changeset)?;
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let now = tx.now_ms();
        let suite = CheckSuite {
            id: tx.new_id()?,
            repo_id: repo_id.into(),
            changeset,
            app_id: cx.actor_id.clone(),
            status: CheckStatus::Queued as i32,
            head_ref: head_ref.into(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            runs_count: 0,
        };
        tx.insert(
            &SUITES,
            &Row::new(repo_id, suite.id.clone())
                .k1(cs)
                .k2(cx.actor_id.clone())
                .n1(now as i64)
                .msg(&suite),
        )
        .await?;
        tx.emit_event(Event {
            kind: "check_suite.created".into(),
            repo_id: repo_id.into(),
            org_id: org_id.into(),
            payload_json: serde_json::to_string(&suite).unwrap_or_default(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("check_suite.create", org_id)
                .repo_id(repo_id)
                .target_id(&suite.id),
        )
        .await?;
        tx.commit().await?;
        Ok(suite)
    }

    /// Suites of a changeset (or all), optionally of one app, with aggregates.
    pub async fn list_suites(
        &self,
        cx: &Cx,
        repo_id: &str,
        changeset_hex: Option<&str>,
        app_id: &str,
    ) -> Result<Vec<CheckSuite>> {
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let mut q = Scan::scope(repo_id, MAX_LIST);
        if let Some(c) = changeset_hex {
            q = q.key(Key::K1(c.to_string()));
        }
        let rows = tx.scan_msgs::<CheckSuite>(&SUITES, &q).await?;
        let mut out = Vec::new();
        for (mut s, _) in rows {
            if !app_id.is_empty() && s.app_id != app_id {
                continue;
            }
            let runs = Self::suite_runs(&mut tx, repo_id, &s.id).await?;
            Self::aggregate(&mut s, &runs);
            out.push(s);
        }
        Ok(out)
    }

    /// Queues a fresh run for each latest run of the suite and emits
    /// `check_suite.rerequested`.
    pub async fn rerequest_suite(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        id: &str,
    ) -> Result<CheckSuite> {
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let Some((mut suite, row)) = tx.get_msg::<CheckSuite>(&SUITES, repo_id, id).await? else {
            return Err(ForgeError::not_found(format!("check suite {id}")));
        };
        let runs = Self::suite_runs(&mut tx, repo_id, id).await?;
        let mut latest: BTreeMap<String, CheckRun> = BTreeMap::new();
        for r in runs {
            latest.entry(r.name.clone()).or_insert(r);
        }
        for r in latest.values() {
            Self::requeue(&mut tx, cx, r).await?;
        }
        suite.updated_at = Some(ts(tx.now_ms()));
        tx.update(&SUITES, &row.clone().msg(&suite)).await?;
        let runs = Self::suite_runs(&mut tx, repo_id, id).await?;
        Self::aggregate(&mut suite, &runs);
        tx.emit_event(Event {
            kind: "check_suite.rerequested".into(),
            repo_id: repo_id.into(),
            org_id: org_id.into(),
            payload_json: serde_json::to_string(&suite).unwrap_or_default(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("check_suite.rerequest", org_id)
                .repo_id(repo_id)
                .target_id(id),
        )
        .await?;
        tx.commit().await?;
        Ok(suite)
    }

    /// Sets a commit status and mirrors it onto the `status:<context>` check run.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_status(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        changeset: Option<ObjectId>,
        context: &str,
        state: CommitStatusState,
        target_url: &str,
        description: &str,
    ) -> Result<CommitStatus> {
        let cs = cs_hex(&changeset)?;
        if state == CommitStatusState::Unspecified {
            return Err(ForgeError::missing("state"));
        }
        let context = if context.is_empty() {
            "default"
        } else {
            context
        };
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let now = tx.now_ms();
        let status = CommitStatus {
            id: tx.new_id()?,
            changeset: changeset.clone(),
            context: context.into(),
            state: state as i32,
            target_url: target_url.into(),
            description: description.into(),
            creator: Some(cx.principal()),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        tx.insert(
            &STATUSES,
            &Row::new(repo_id, status.id.clone())
                .k1(cs.clone())
                .k2(context)
                .n1(now as i64)
                .msg(&status),
        )
        .await?;
        let check_status = match state {
            CommitStatusState::Success => CheckStatus::Passed,
            CommitStatusState::Failure | CommitStatusState::Error => CheckStatus::Failed,
            _ => CheckStatus::Running,
        };
        let external = format!("status:{context}");
        let existing = tx
            .scan_msgs::<CheckRun>(
                &RUNS,
                &Scan::scope(repo_id, MAX_LIST).key(Key::K1(cs.clone())),
            )
            .await?
            .into_iter()
            .find(|(r, _)| r.external_id == external && r.name == context);
        let run = match existing {
            Some((mut run, row)) => {
                run.status = check_status as i32;
                run.url = target_url.into();
                run.summary = description.into();
                run.finished_at = terminal(check_status).then(|| ts(now));
                tx.update(&RUNS, &row.clone().msg(&run)).await?;
                run
            }
            None => {
                let run = CheckRun {
                    id: tx.new_id()?,
                    repo_id: repo_id.into(),
                    changeset,
                    name: context.into(),
                    status: check_status as i32,
                    url: target_url.into(),
                    summary: description.into(),
                    started_at: Some(ts(now)),
                    finished_at: terminal(check_status).then(|| ts(now)),
                    external_id: external,
                    reporter: Some(cx.principal()),
                    ..Default::default()
                };
                tx.insert(
                    &RUNS,
                    &Row::new(repo_id, run.id.clone())
                        .k1(cs)
                        .n1(now as i64)
                        .msg(&run),
                )
                .await?;
                run
            }
        };
        Self::emit(&mut tx, "check.updated", org_id, &run).await?;
        tx.emit_event(Event {
            kind: "status.created".into(),
            repo_id: repo_id.into(),
            org_id: org_id.into(),
            payload_json: serde_json::to_string(&status).unwrap_or_default(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("status.create", org_id)
                .repo_id(repo_id)
                .target_id(&status.id)
                .after(
                    &serde_json::json!({"context": context, "state": state.as_str_name()})
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(status)
    }

    /// Statuses of a changeset, newest first, paged by id.
    pub async fn list_statuses(
        &self,
        cx: &Cx,
        repo_id: &str,
        changeset_hex: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<CommitStatus>, String)> {
        let mut tx = self.db.begin(Surface::CHECKS, cx).await?;
        let cursor = (!after.is_empty()).then(|| Cursor {
            n1: 0,
            scope: repo_id.to_string(),
            id: after.to_string(),
        });
        let rows = tx
            .scan_msgs::<CommitStatus>(
                &STATUSES,
                &Scan::scope(repo_id, limit + 1)
                    .desc()
                    .key(Key::K1(changeset_hex.to_string()))
                    .after(cursor),
            )
            .await?;
        let more = rows.len() > limit;
        let out: Vec<CommitStatus> = rows.into_iter().take(limit).map(|(s, _)| s).collect();
        let next = if more {
            out.last().map(|s| s.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((out, next))
    }

    /// Latest status per context and their combination (failure if any failed, pending if
    /// any pending or none, else success).
    pub async fn combined_status(
        &self,
        cx: &Cx,
        repo_id: &str,
        changeset: ObjectId,
    ) -> Result<CombinedStatus> {
        let cs = hex(&changeset.raw);
        let (all, _) = self.list_statuses(cx, repo_id, &cs, "", MAX_LIST).await?;
        let mut latest: BTreeMap<String, CommitStatus> = BTreeMap::new();
        for s in all {
            latest.entry(s.context.clone()).or_insert(s);
        }
        let statuses: Vec<CommitStatus> = latest.into_values().collect();
        let states: Vec<CommitStatusState> = statuses
            .iter()
            .map(|s| CommitStatusState::try_from(s.state).unwrap_or(CommitStatusState::Pending))
            .collect();
        let state = if states
            .iter()
            .any(|s| matches!(s, CommitStatusState::Failure | CommitStatusState::Error))
        {
            CommitStatusState::Failure
        } else if states.is_empty() || states.contains(&CommitStatusState::Pending) {
            CommitStatusState::Pending
        } else {
            CommitStatusState::Success
        };
        Ok(CombinedStatus {
            changeset: Some(changeset),
            state: state as i32,
            statuses,
        })
    }

    /// Latest run per name of a changeset, as policy input for required checks.
    pub async fn check_results(
        &self,
        cx: &Cx,
        repo_id: &str,
        changeset_hex: &str,
    ) -> Result<Vec<bee_policy::CheckResult>> {
        let runs = self
            .list(
                cx,
                repo_id,
                &RunFilter {
                    changeset_hex: Some(changeset_hex.into()),
                    latest_only: true,
                    ..RunFilter::default()
                },
            )
            .await?;
        Ok(runs
            .iter()
            .map(|r| bee_policy::CheckResult {
                name: r.name.clone(),
                status: policy_status(status_of(r)),
            })
            .collect())
    }

    /// Latest `post_land` runs of a changeset that failed.
    pub async fn failing_post_land(
        &self,
        cx: &Cx,
        repo_id: &str,
        changeset_hex: &str,
    ) -> Result<Vec<CheckRun>> {
        let runs = self
            .list(
                cx,
                repo_id,
                &RunFilter {
                    changeset_hex: Some(changeset_hex.into()),
                    latest_only: true,
                    ..RunFilter::default()
                },
            )
            .await?;
        Ok(runs
            .into_iter()
            .filter(|r| {
                r.post_land && policy_status(status_of(r)) == bee_policy::CheckStatus::Failure
            })
            .collect())
    }
}

fn run_json(r: &CheckRun) -> String {
    serde_json::json!({
        "id": r.id,
        "name": r.name,
        "status": status_of(r).as_str_name(),
        "post_land": r.post_land,
        "started_at_ms": r.started_at.as_ref().map(ts_ms),
        "finished_at_ms": r.finished_at.as_ref().map(ts_ms),
    })
    .to_string()
}
