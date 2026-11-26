//! Eval runs.
//!
//! ```text
//! QUEUED --progress--> RUNNING --result--> SUCCEEDED | FAILED
//!    \________________cancel_________________/   (CANCELLED; a finished run cannot be cancelled)
//! ```
//!
//! `RunPromptEval` only queues a run. The executor reports with
//! [`Models::record_eval_progress`] (which starts it) and [`Models::record_eval_result`]. When
//! the run asked for it, the result is reported as a check run on the run's changeset through
//! the checks module: it passes when the run succeeded and every evaluator's mean score is at
//! least the run's `pass_threshold`.

use bee_proto::v1::{
    CheckOutput, CheckStatus, Cost, EvalRun, EvalRunState, EvalScore, ModelInfo, ObjectId,
    PromptFile, Repo, RunPromptEvalRequest,
};

use super::prompts::path_str;
use super::records::EvalRow;
use super::{CATALOG, Change, EVALS, Models, PROMPTS, SURFACE, record};
use crate::checks::Report;
use crate::cx::Cx;
use crate::enterprise::common::{MAX_LIST, SCAN_CAP, get_or_404, json, principal_of};
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Key, Row, Scan, Tx};

/// Most models one run compares.
pub const MAX_EVAL_MODELS: usize = 20;

/// Filters of [`Models::list_eval_runs`]; unset fields match everything.
#[derive(Clone, Debug, Default)]
pub struct EvalFilter {
    /// Only runs of this prompt file.
    pub prompt_path: Vec<u8>,
    /// Only runs of this changeset.
    pub changeset: Option<ObjectId>,
    /// Only runs in this state.
    pub state: EvalRunState,
}

/// The executor's final report of a run.
#[derive(Clone, Debug, Default)]
pub struct EvalResult {
    /// Score per evaluator.
    pub scores: Vec<EvalScore>,
    /// Cases in the dataset.
    pub cases_total: u32,
    /// Cases evaluated.
    pub cases_done: u32,
    /// Tokens and cost of the run.
    pub cost: Option<Cost>,
    /// Signed URL of the per-case results.
    pub results_url: String,
    /// Why the run failed; non-empty makes the run `FAILED`.
    pub error: String,
}

fn state_of(r: &EvalRun) -> EvalRunState {
    EvalRunState::try_from(r.state).unwrap_or(EvalRunState::Unspecified)
}

fn finished(s: EvalRunState) -> bool {
    matches!(
        s,
        EvalRunState::Succeeded | EvalRunState::Failed | EvalRunState::Cancelled
    )
}

fn run_of(row: &EvalRow) -> EvalRun {
    row.run.clone().unwrap_or_default()
}

impl Models {
    /// Queues an eval of an indexed prompt file. `changeset` is the revision's changeset (the
    /// service resolves it); a run that reports a check needs one. `NOT_FOUND` when the file
    /// is not indexed or a model is not in the catalog.
    pub async fn run_prompt_eval(
        &self,
        cx: &Cx,
        repo: &Repo,
        changeset: Option<ObjectId>,
        req: &RunPromptEvalRequest,
    ) -> Result<EvalRun> {
        let path = path_str(&req.prompt_path)?;
        if path.is_empty() {
            return Err(ForgeError::missing("prompt_path"));
        }
        let dataset = path_str(&req.dataset_path)?;
        if !req.pass_threshold.is_finite() || !(0.0..=1.0).contains(&req.pass_threshold) {
            return Err(ForgeError::invalid(
                "invalid_threshold",
                "pass_threshold must be between 0 and 1",
            ));
        }
        if req.report_check && changeset.is_none() {
            return Err(ForgeError::invalid(
                "changeset_required",
                "reporting a check needs a revision that names a changeset",
            ));
        }
        if req.check_name.len() > 200 {
            return Err(ForgeError::invalid(
                "invalid_name",
                "check_name over 200 bytes",
            ));
        }
        let started_by = principal_of(&self.forge, &cx.actor_id).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (prompt, _) =
            get_or_404::<PromptFile>(&mut tx, &PROMPTS, &repo.id, &path, "prompt file").await?;
        let mut models: Vec<String> = Vec::new();
        for m in if req.models.is_empty() {
            vec![prompt.model.clone()]
        } else {
            req.models.clone()
        } {
            if !models.contains(&m) {
                models.push(m);
            }
        }
        if models.len() > MAX_EVAL_MODELS {
            return Err(ForgeError::too_large(
                "eval_models",
                MAX_EVAL_MODELS as u64,
                models.len() as u64,
            ));
        }
        for m in &models {
            get_or_404::<ModelInfo>(&mut tx, &CATALOG, "", m, "model").await?;
        }
        let now = tx.now_ms();
        let run = EvalRun {
            id: tx.new_id()?,
            repo: Some(bee_proto::v1::RepoRef {
                repo: Some(bee_proto::v1::repo_ref::Repo::Id(repo.id.clone())),
            }),
            changeset: changeset.clone(),
            prompt_path: req.prompt_path.clone(),
            dataset_path: req.dataset_path.clone(),
            models,
            state: EvalRunState::Queued as i32,
            scores: vec![],
            cases_total: if dataset.is_empty() {
                prompt.test_data_count
            } else {
                0
            },
            cases_done: 0,
            cost: Some(Cost::default()),
            check_run_id: String::new(),
            results_url: String::new(),
            error: String::new(),
            started_by: Some(started_by),
            created_at: Some(ts(now)),
            finished_at: None,
        };
        let row = EvalRow {
            run: Some(run.clone()),
            org_id: repo.org_id.clone(),
            report_check: req.report_check,
            check_name: if req.report_check && req.check_name.is_empty() {
                format!("prompt-eval: {}", prompt.name)
            } else {
                req.check_name.clone()
            },
            pass_threshold: req.pass_threshold,
            check_error: String::new(),
        };
        let mut r = Row::new(repo.id.clone(), run.id.clone())
            .k1(path)
            .n1(now as i64);
        if let Some(cs) = &changeset {
            r = r.k2(hex(&cs.raw));
        }
        tx.insert(&EVALS, &r.msg(&row)).await?;
        record(
            &mut tx,
            Change {
                kind: "model.eval_queued",
                action: "model.eval.run",
                org_id: &repo.org_id,
                repo_id: &repo.id,
                target: &run.id,
                before: None,
                after: &json(&run),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    async fn load(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(EvalRow, Row)> {
        get_or_404::<EvalRow>(tx, &EVALS, repo_id, id, "eval run").await
    }

    /// The executor reports progress: starts a queued run and updates the case counts.
    /// `INVALID_STATE` once the run finished or was cancelled.
    pub async fn record_eval_progress(
        &self,
        cx: &Cx,
        repo_id: &str,
        id: &str,
        cases_total: Option<u32>,
        cases_done: u32,
    ) -> Result<EvalRun> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut row, meta) = Self::load(&mut tx, repo_id, id).await?;
        let mut run = run_of(&row);
        let before = json(&run);
        if finished(state_of(&run)) {
            return Err(ForgeError::invalid_state(format!(
                "eval run {id} is {}",
                state_of(&run).as_str_name()
            )));
        }
        let total = cases_total.unwrap_or(run.cases_total);
        if total > 0 && cases_done > total {
            return Err(ForgeError::invalid(
                "invalid_progress",
                "cases_done is above cases_total",
            ));
        }
        if cases_done < run.cases_done {
            return Err(ForgeError::invalid(
                "invalid_progress",
                "cases_done cannot go down",
            ));
        }
        run.cases_total = total;
        run.cases_done = cases_done;
        run.state = EvalRunState::Running as i32;
        row.run = Some(run.clone());
        tx.update(&EVALS, &meta.msg(&row)).await?;
        record(
            &mut tx,
            Change {
                kind: "model.eval_updated",
                action: "model.eval.progress",
                org_id: &row.org_id,
                repo_id,
                target: id,
                before: Some(&before),
                after: &json(&run),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    /// The executor reports the result of a running run: `SUCCEEDED`, or `FAILED` when
    /// `error` is set. `INVALID_STATE` unless the run is running. Reports the check run when
    /// the run asked for one; a failure to report is kept on the record, not returned.
    pub async fn record_eval_result(
        &self,
        cx: &Cx,
        repo_id: &str,
        id: &str,
        result: EvalResult,
    ) -> Result<EvalRun> {
        for s in &result.scores {
            if s.evaluator.is_empty() || !s.mean.is_finite() {
                return Err(ForgeError::invalid(
                    "invalid_score",
                    "scores need an evaluator and a finite mean",
                ));
            }
        }
        if result.cases_total > 0 && result.cases_done > result.cases_total {
            return Err(ForgeError::invalid(
                "invalid_progress",
                "cases_done is above cases_total",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut row, meta) = Self::load(&mut tx, repo_id, id).await?;
        let mut run = run_of(&row);
        let before = json(&run);
        match state_of(&run) {
            EvalRunState::Running => {}
            EvalRunState::Queued => {
                return Err(ForgeError::invalid_state(format!(
                    "eval run {id} has not started"
                )));
            }
            s => {
                return Err(ForgeError::invalid_state(format!(
                    "eval run {id} is {}",
                    s.as_str_name()
                )));
            }
        }
        let failed = !result.error.is_empty();
        run.state = if failed {
            EvalRunState::Failed
        } else {
            EvalRunState::Succeeded
        } as i32;
        run.scores = result.scores;
        run.cases_total = result.cases_total.max(run.cases_total);
        run.cases_done = result.cases_done;
        run.cost = Some(result.cost.unwrap_or_default());
        run.results_url = result.results_url;
        run.error = result.error;
        run.finished_at = Some(ts(tx.now_ms()));
        row.run = Some(run.clone());
        tx.update(&EVALS, &meta.msg(&row)).await?;
        record(
            &mut tx,
            Change {
                kind: "model.eval_updated",
                action: "model.eval.result",
                org_id: &row.org_id,
                repo_id,
                target: id,
                before: Some(&before),
                after: &json(&run),
            },
        )
        .await?;
        tx.commit().await?;
        if !row.report_check || run.changeset.is_none() {
            return Ok(run);
        }
        let passed = !failed && run.scores.iter().all(|s| s.mean >= row.pass_threshold);
        let summary = if failed {
            format!("eval failed: {}", run.error)
        } else {
            let mut lines: Vec<String> = run
                .scores
                .iter()
                .map(|s| {
                    format!(
                        "{}: mean {:.3} ({} passed, {} failed)",
                        s.evaluator, s.mean, s.passed, s.failed
                    )
                })
                .collect();
            lines.push(format!("pass threshold {:.3}", row.pass_threshold));
            lines.join("; ")
        };
        let reported = self
            .forge
            .checks
            .report(
                cx,
                &row.org_id,
                repo_id,
                Report {
                    changeset: run.changeset.clone(),
                    name: row.check_name.clone(),
                    status: if passed {
                        CheckStatus::Passed
                    } else {
                        CheckStatus::Failed
                    },
                    url: run.results_url.clone(),
                    summary: summary.clone(),
                    external_id: format!("eval:{id}"),
                    output: Some(CheckOutput {
                        title: row.check_name.clone(),
                        summary,
                        ..Default::default()
                    }),
                    ..Report::default()
                },
            )
            .await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut row, meta) = Self::load(&mut tx, repo_id, id).await?;
        let mut run = run_of(&row);
        let before = json(&run);
        match reported {
            Ok(check) => run.check_run_id = check.id,
            Err(e) => row.check_error = e.to_string(),
        }
        row.run = Some(run.clone());
        tx.update(&EVALS, &meta.msg(&row)).await?;
        record(
            &mut tx,
            Change {
                kind: "model.eval_updated",
                action: "model.eval.check_reported",
                org_id: &row.org_id,
                repo_id,
                target: id,
                before: Some(&before),
                after: &json(&run),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    /// Cancels a queued or running run; `INVALID_STATE` when it already finished.
    pub async fn cancel_eval_run(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<EvalRun> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut row, meta) = Self::load(&mut tx, repo_id, id).await?;
        let mut run = run_of(&row);
        let before = json(&run);
        if finished(state_of(&run)) {
            return Err(ForgeError::invalid_state(format!(
                "eval run {id} already finished ({})",
                state_of(&run).as_str_name()
            )));
        }
        run.state = EvalRunState::Cancelled as i32;
        run.finished_at = Some(ts(tx.now_ms()));
        row.run = Some(run.clone());
        tx.update(&EVALS, &meta.msg(&row)).await?;
        record(
            &mut tx,
            Change {
                kind: "model.eval_updated",
                action: "model.eval.cancel",
                org_id: &row.org_id,
                repo_id,
                target: id,
                before: Some(&before),
                after: &json(&run),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(run)
    }

    /// One eval run.
    pub async fn get_eval_run(&self, cx: &Cx, repo_id: &str, id: &str) -> Result<EvalRun> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (row, _) = Self::load(&mut tx, repo_id, id).await?;
        Ok(run_of(&row))
    }

    /// Eval runs of a repo, newest first, optionally of one prompt file, changeset or state;
    /// returns the page and the next token (the id of the last run).
    pub async fn list_eval_runs(
        &self,
        cx: &Cx,
        repo_id: &str,
        filter: &EvalFilter,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<EvalRun>, String)> {
        let path = path_str(&filter.prompt_path)?;
        let (changeset, state) = (&filter.changeset, filter.state);
        let mut q = Scan::scope(repo_id, SCAN_CAP).by_n1().desc();
        if !path.is_empty() {
            q = q.key(Key::K1(path));
        } else if let Some(cs) = changeset.as_ref().filter(|c| !c.raw.is_empty()) {
            q = q.key(Key::K2(hex(&cs.raw)));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx.scan_msgs::<EvalRow>(&EVALS, &q).await?;
        let mut runs: Vec<EvalRun> = rows
            .into_iter()
            .map(|(r, _)| run_of(&r))
            .filter(|r| {
                changeset
                    .as_ref()
                    .filter(|c| !c.raw.is_empty())
                    .is_none_or(|c| r.changeset.as_ref().is_some_and(|x| x.raw == c.raw))
            })
            .filter(|r| state == EvalRunState::Unspecified || r.state == state as i32)
            .collect();
        if !after.is_empty() {
            match runs.iter().position(|r| r.id == after) {
                Some(i) => {
                    runs.drain(..=i);
                }
                None => runs.clear(),
            }
        }
        let limit = limit.clamp(1, MAX_LIST);
        let next = if runs.len() > limit {
            runs.truncate(limit);
            runs.last().map(|r| r.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((runs, next))
    }
}
