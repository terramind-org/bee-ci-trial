//! The job graph: ordering, cycle detection, and how job results propagate through `needs`.
//!
//! Rules (GitHub's "Using jobs" and "Evaluate expressions" docs):
//!
//! * A job starts when every job it needs has finished.
//! * Its `if` (default `success()`) is evaluated with the status functions bound to its
//!   dependency chain: `success()` is true when every needed job succeeded and no ancestor
//!   failed; `failure()` when any ancestor failed; `cancelled()` when the run was cancelled.
//!   So a failed or skipped job skips its dependents unless they use `always()`, `failure()`
//!   or another condition that does not require success.
//! * A job that fails with `continue-on-error: true` reports `success` to its dependents and
//!   does not fail the run.
//! * A matrix job's result is `failure` if any leg failed (without `continue-on-error`),
//!   else `cancelled` if any leg was cancelled, else `skipped` if every leg was skipped,
//!   else `success`. Its outputs are merged in completion order, later non-empty values win.
//! * With `fail-fast` (the default) a failing leg cancels the legs still queued or running.

use std::collections::BTreeMap;

use crate::expr::{Condition, ContextProvider, ExprError, Status};
use crate::model::Job;
use crate::value::{Object, Value};

/// Orders jobs so every job comes after the jobs it needs. Ties keep source order. On a
/// cycle, returns the jobs on it, starting and ending with the same id.
pub fn topo_order(jobs: &[Job]) -> Result<Vec<String>, Vec<String>> {
    let index = |id: &str| jobs.iter().position(|j| j.id.eq_ignore_ascii_case(id));
    let n = jobs.len();
    let mut indegree = vec![0usize; n];
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, j) in jobs.iter().enumerate() {
        let mut seen = Vec::new();
        for need in &j.needs {
            if let Some(d) = index(need)
                && d != i
                && !seen.contains(&d)
            {
                seen.push(d);
                edges[d].push(i);
                indegree[i] += 1;
            }
        }
    }
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while let Some(next) = (0..n).find(|&i| !done[i] && indegree[i] == 0) {
        done[next] = true;
        order.push(jobs[next].id.clone());
        for &m in &edges[next] {
            indegree[m] -= 1;
        }
    }
    if order.len() == n {
        return Ok(order);
    }
    // Find a cycle among the remaining jobs by walking needs.
    let start = (0..n).find(|&i| !done[i]).unwrap_or(0);
    let mut path: Vec<usize> = vec![start];
    loop {
        let cur = *path.last().unwrap_or(&start);
        let next = jobs[cur]
            .needs
            .iter()
            .filter_map(|x| index(x))
            .find(|&d| !done[d] && d != cur);
        let Some(next) = next else {
            return Err(vec![jobs[start].id.clone()]);
        };
        if let Some(p) = path.iter().position(|&x| x == next) {
            let mut cycle: Vec<String> = path[p..].iter().map(|&i| jobs[i].id.clone()).collect();
            cycle.reverse();
            cycle.rotate_right(1);
            cycle.push(cycle[0].clone());
            return Err(cycle);
        }
        path.push(next);
    }
}

/// The result of a finished job, as `needs.<id>.result` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobResult {
    /// Succeeded (or failed with `continue-on-error`).
    Success,
    /// Failed.
    Failure,
    /// Cancelled.
    Cancelled,
    /// Not run because its condition was false.
    Skipped,
}

impl JobResult {
    /// The context spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            JobResult::Success => "success",
            JobResult::Failure => "failure",
            JobResult::Cancelled => "cancelled",
            JobResult::Skipped => "skipped",
        }
    }
}

/// How one matrix leg (or a non-matrix job) finished.
#[derive(Clone, Debug, PartialEq)]
pub struct LegOutcome {
    /// The leg's own conclusion before `continue-on-error`.
    pub result: JobResult,
    /// Whether the leg had `continue-on-error: true`.
    pub continue_on_error: bool,
    /// The leg's outputs.
    pub outputs: Object,
}

/// A finished job as its dependents see it.
#[derive(Clone, Debug, PartialEq)]
pub struct JobOutcome {
    /// `needs.<id>.result`.
    pub result: JobResult,
    /// `needs.<id>.outputs`.
    pub outputs: Object,
}

/// Combines matrix legs, given in completion order, into the job's outcome.
pub fn aggregate_legs(legs: &[LegOutcome]) -> JobOutcome {
    let effective = |l: &LegOutcome| {
        if l.result == JobResult::Failure && l.continue_on_error {
            JobResult::Success
        } else {
            l.result
        }
    };
    let result = if legs.iter().any(|l| effective(l) == JobResult::Failure) {
        JobResult::Failure
    } else if legs.iter().any(|l| effective(l) == JobResult::Cancelled) {
        JobResult::Cancelled
    } else if !legs.is_empty() && legs.iter().all(|l| l.result == JobResult::Skipped) {
        JobResult::Skipped
    } else {
        JobResult::Success
    };
    let mut outputs = Object::new();
    for l in legs {
        for (k, v) in l.outputs.iter() {
            if !matches!(v, Value::String(s) if s.is_empty()) || !outputs.contains_key(k) {
                outputs.insert(k.to_owned(), v.clone());
            }
        }
    }
    JobOutcome { result, outputs }
}

/// Which still-unfinished legs to cancel after `failed_leg` failed.
pub fn fail_fast_cancellations(
    fail_fast: bool,
    failed_leg: &LegOutcome,
    unfinished: &[usize],
) -> Vec<usize> {
    if fail_fast && failed_leg.result == JobResult::Failure && !failed_leg.continue_on_error {
        unfinished.to_vec()
    } else {
        Vec::new()
    }
}

/// The state of one job in a running workflow.
#[derive(Clone, Debug, PartialEq)]
pub enum JobState {
    /// Waiting for its needs.
    Waiting,
    /// Started.
    Running,
    /// Finished.
    Done(JobOutcome),
}

/// What to do with a job whose needs have all finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Its condition is true: start it.
    Run,
    /// Its condition is false: it is skipped.
    Skip,
}

struct GraphJob {
    id: String,
    needs: Vec<String>,
    condition: Condition,
}

/// Drives one workflow run's jobs through the graph.
pub struct RunGraph {
    jobs: Vec<GraphJob>,
    order: Vec<String>,
    state: BTreeMap<String, JobState>,
    cancelled: bool,
    base: Object,
}

struct JobCtx<'a> {
    base: &'a Object,
    needs: Value,
    status: Status,
}

impl ContextProvider for JobCtx<'_> {
    fn context(&self, name: &str) -> Option<Value> {
        if name == "needs" {
            return Some(self.needs.clone());
        }
        self.base.get(name).cloned()
    }

    fn status(&self) -> Status {
        self.status
    }
}

impl RunGraph {
    /// Builds the graph for a workflow's jobs. `base` holds the contexts job conditions can
    /// read besides `needs` (`github`, `vars`, `inputs`).
    pub fn new(jobs: &[Job], base: Object) -> Result<RunGraph, Vec<String>> {
        let order = topo_order(jobs)?;
        let jobs: Vec<GraphJob> = jobs
            .iter()
            .map(|j| GraphJob {
                id: j.id.clone(),
                needs: j.needs.clone(),
                condition: j
                    .condition
                    .clone()
                    .unwrap_or_else(Condition::default_success),
            })
            .collect();
        let state = jobs
            .iter()
            .map(|j| (j.id.clone(), JobState::Waiting))
            .collect();
        Ok(RunGraph {
            jobs,
            order,
            state,
            cancelled: false,
            base,
        })
    }

    fn job(&self, id: &str) -> Option<&GraphJob> {
        self.jobs.iter().find(|j| j.id.eq_ignore_ascii_case(id))
    }

    fn outcome(&self, id: &str) -> Option<&JobOutcome> {
        let key = self.job(id)?.id.clone();
        match self.state.get(&key) {
            Some(JobState::Done(o)) => Some(o),
            _ => None,
        }
    }

    /// Jobs that are waiting and whose needs have all finished, in topological order.
    pub fn ready(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| matches!(self.state.get(*id), Some(JobState::Waiting)))
            .filter(|id| {
                self.job(id)
                    .is_some_and(|j| j.needs.iter().all(|n| self.outcome(n).is_some()))
            })
            .cloned()
            .collect()
    }

    fn ancestors(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut stack: Vec<String> = self.job(id).map(|j| j.needs.clone()).unwrap_or_default();
        while let Some(n) = stack.pop() {
            let Some(j) = self.job(&n) else { continue };
            if out.iter().any(|o| o == &j.id) {
                continue;
            }
            out.push(j.id.clone());
            stack.extend(j.needs.iter().cloned());
        }
        out.sort();
        out
    }

    /// The status functions as a job's condition sees them.
    pub fn status_for(&self, id: &str) -> Status {
        let Some(job) = self.job(id) else {
            return Status::SUCCESS;
        };
        let failed = self.ancestors(id).iter().any(|a| {
            self.outcome(a)
                .is_some_and(|o| o.result == JobResult::Failure)
        });
        let needs_ok = job.needs.iter().all(|n| {
            self.outcome(n)
                .is_some_and(|o| o.result == JobResult::Success)
        });
        Status {
            success: needs_ok && !failed && !self.cancelled,
            failure: failed && !self.cancelled,
            cancelled: self.cancelled,
        }
    }

    /// The `needs` context for a job: its direct dependencies' results and outputs.
    pub fn needs_context(&self, id: &str) -> Value {
        let mut o = Object::new();
        if let Some(job) = self.job(id) {
            for n in &job.needs {
                if let (Some(dep), Some(out)) = (self.job(n), self.outcome(n)) {
                    o.insert(
                        dep.id.clone(),
                        Value::object([
                            ("result", Value::from(out.result.as_str())),
                            ("outputs", Value::from(out.outputs.clone())),
                        ]),
                    );
                }
            }
        }
        Value::from(o)
    }

    /// Evaluates a ready job's condition. A skipped job is recorded as finished.
    pub fn decide(&mut self, id: &str) -> Result<Decision, ExprError> {
        let Some(job) = self.job(id) else {
            return Err(ExprError {
                message: format!("unknown job '{id}'"),
                offset: None,
            });
        };
        let key = job.id.clone();
        let ctx = JobCtx {
            base: &self.base,
            needs: self.needs_context(&key),
            status: self.status_for(&key),
        };
        let run = job.condition.evaluate(&ctx)?;
        if run {
            self.state.insert(key, JobState::Running);
            Ok(Decision::Run)
        } else {
            self.state.insert(
                key,
                JobState::Done(JobOutcome {
                    result: JobResult::Skipped,
                    outputs: Object::new(),
                }),
            );
            Ok(Decision::Skip)
        }
    }

    /// Records that a running job finished.
    pub fn complete(&mut self, id: &str, outcome: JobOutcome) {
        if let Some(job) = self.job(id) {
            let key = job.id.clone();
            self.state.insert(key, JobState::Done(outcome));
        }
    }

    /// Marks the run cancelled: later conditions see `cancelled()` as true.
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// The state of a job.
    pub fn state(&self, id: &str) -> Option<&JobState> {
        self.job(id).and_then(|j| self.state.get(&j.id))
    }

    /// Whether every job has finished.
    pub fn is_finished(&self) -> bool {
        self.state.values().all(|s| matches!(s, JobState::Done(_)))
    }

    /// The run's conclusion once finished: `cancelled` if cancelled, `failure` if any job
    /// failed, else `success`.
    pub fn conclusion(&self) -> Option<JobResult> {
        if !self.is_finished() {
            return None;
        }
        if self.cancelled {
            return Some(JobResult::Cancelled);
        }
        let any = |r: JobResult| {
            self.state
                .values()
                .any(|s| matches!(s, JobState::Done(o) if o.result == r))
        };
        Some(if any(JobResult::Failure) {
            JobResult::Failure
        } else if any(JobResult::Cancelled) {
            JobResult::Cancelled
        } else {
            JobResult::Success
        })
    }

    /// Runs the graph to completion with a function that executes each started job,
    /// returning every job's outcome in the order decided. Useful for simulation and tests.
    pub fn simulate(
        &mut self,
        mut run: impl FnMut(&str, &Value) -> JobOutcome,
    ) -> Result<Vec<(String, JobOutcome)>, ExprError> {
        let mut log = Vec::new();
        loop {
            let ready = self.ready();
            if ready.is_empty() {
                break;
            }
            for id in ready {
                match self.decide(&id)? {
                    Decision::Run => {
                        let needs = self.needs_context(&id);
                        let outcome = run(&id, &needs);
                        self.complete(&id, outcome.clone());
                        log.push((id, outcome));
                    }
                    Decision::Skip => {
                        if let Some(JobState::Done(o)) = self.state(&id) {
                            log.push((id.clone(), o.clone()));
                        }
                    }
                }
            }
        }
        Ok(log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_workflow;

    fn jobs(src: &str) -> Vec<Job> {
        parse_workflow("w.yml", src).unwrap().jobs
    }

    const DIAMOND: &str = "on: push\njobs:\n  d:\n    needs: [b, c]\n    runs-on: x\n    steps: [{run: x}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: x}]\n  c:\n    needs: a\n    runs-on: x\n    steps: [{run: x}]\n  a:\n    runs-on: x\n    steps: [{run: x}]\n";

    fn ok() -> JobOutcome {
        JobOutcome {
            result: JobResult::Success,
            outputs: Object::new(),
        }
    }

    fn fail() -> JobOutcome {
        JobOutcome {
            result: JobResult::Failure,
            outputs: Object::new(),
        }
    }

    #[test]
    fn topological_order_keeps_source_order_for_ties() {
        assert_eq!(
            topo_order(&jobs(DIAMOND)).unwrap(),
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn cycles_are_reported_as_a_path() {
        let mut js = jobs(DIAMOND);
        js[3].needs = vec!["d".into()];
        let cycle = topo_order(&js).unwrap_err();
        assert_eq!(cycle.first(), cycle.last());
        assert!(cycle.len() >= 3);
    }

    #[test]
    fn failure_skips_dependents_unless_always() {
        let src = "on: push\njobs:\n  build:\n    runs-on: x\n    steps: [{run: x}]\n  test:\n    needs: build\n    runs-on: x\n    steps: [{run: x}]\n  report:\n    needs: test\n    if: always()\n    runs-on: x\n    steps: [{run: x}]\n  notify:\n    needs: test\n    if: failure()\n    runs-on: x\n    steps: [{run: x}]\n  deploy:\n    needs: test\n    runs-on: x\n    steps: [{run: x}]\n";
        let mut g = RunGraph::new(&jobs(src), Object::new()).unwrap();
        let log = g
            .simulate(|id, _| if id == "build" { fail() } else { ok() })
            .unwrap();
        let result = |id: &str| log.iter().find(|(j, _)| j == id).unwrap().1.result;
        assert_eq!(result("build"), JobResult::Failure);
        assert_eq!(result("test"), JobResult::Skipped);
        assert_eq!(result("report"), JobResult::Success);
        // failure() looks at the whole ancestor chain.
        assert_eq!(result("notify"), JobResult::Success);
        assert_eq!(result("deploy"), JobResult::Skipped);
        assert_eq!(g.conclusion(), Some(JobResult::Failure));
    }

    #[test]
    fn needs_context_and_conditions_on_outputs() {
        let src = "on: push\njobs:\n  a:\n    runs-on: x\n    outputs:\n      go: ${{ steps.s.outputs.go }}\n    steps: [{id: s, run: x}]\n  b:\n    needs: a\n    if: needs.a.outputs.go == 'yes'\n    runs-on: x\n    steps: [{run: x}]\n  c:\n    needs: a\n    if: needs.a.outputs.go == 'no'\n    runs-on: x\n    steps: [{run: x}]\n";
        let mut g = RunGraph::new(&jobs(src), Object::new()).unwrap();
        let log = g
            .simulate(|id, needs| {
                if id == "a" {
                    let mut o = Object::new();
                    o.insert("go", Value::from("yes"));
                    JobOutcome {
                        result: JobResult::Success,
                        outputs: o,
                    }
                } else {
                    assert_eq!(
                        needs.get("a").unwrap().get("result").unwrap(),
                        &Value::from("success")
                    );
                    ok()
                }
            })
            .unwrap();
        assert_eq!(log[1], ("b".to_owned(), ok()));
        assert_eq!(log[2].1.result, JobResult::Skipped);
    }

    #[test]
    fn cancellation_runs_only_always_and_cancelled_jobs() {
        let src = "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: x}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: x}]\n  c:\n    needs: a\n    if: cancelled()\n    runs-on: x\n    steps: [{run: x}]\n  d:\n    needs: a\n    if: always()\n    runs-on: x\n    steps: [{run: x}]\n";
        let mut g = RunGraph::new(&jobs(src), Object::new()).unwrap();
        assert_eq!(g.ready(), vec!["a"]);
        assert_eq!(g.decide("a").unwrap(), Decision::Run);
        g.cancel();
        g.complete(
            "a",
            JobOutcome {
                result: JobResult::Cancelled,
                outputs: Object::new(),
            },
        );
        assert_eq!(g.ready(), vec!["b", "c", "d"]);
        assert_eq!(g.decide("b").unwrap(), Decision::Skip);
        assert_eq!(g.decide("c").unwrap(), Decision::Run);
        assert_eq!(g.decide("d").unwrap(), Decision::Run);
        g.complete("c", ok());
        g.complete("d", ok());
        assert_eq!(g.conclusion(), Some(JobResult::Cancelled));
    }

    #[test]
    fn matrix_aggregation_and_continue_on_error() {
        let leg = |r, coe| LegOutcome {
            result: r,
            continue_on_error: coe,
            outputs: Object::new(),
        };
        assert_eq!(
            aggregate_legs(&[
                leg(JobResult::Success, false),
                leg(JobResult::Failure, true)
            ])
            .result,
            JobResult::Success
        );
        assert_eq!(
            aggregate_legs(&[
                leg(JobResult::Success, false),
                leg(JobResult::Failure, false)
            ])
            .result,
            JobResult::Failure
        );
        assert_eq!(
            aggregate_legs(&[
                leg(JobResult::Success, false),
                leg(JobResult::Cancelled, false)
            ])
            .result,
            JobResult::Cancelled
        );
        assert_eq!(
            aggregate_legs(&[leg(JobResult::Skipped, false)]).result,
            JobResult::Skipped
        );
        let mut a = Object::new();
        a.insert("v", Value::from("1"));
        let mut b = Object::new();
        b.insert("v", Value::from("2"));
        let legs = [
            LegOutcome {
                result: JobResult::Success,
                continue_on_error: false,
                outputs: a,
            },
            LegOutcome {
                result: JobResult::Success,
                continue_on_error: false,
                outputs: b,
            },
        ];
        assert_eq!(
            aggregate_legs(&legs).outputs.get("v"),
            Some(&Value::from("2"))
        );
        assert_eq!(
            fail_fast_cancellations(true, &leg(JobResult::Failure, false), &[2, 3]),
            vec![2, 3]
        );
        assert!(fail_fast_cancellations(false, &leg(JobResult::Failure, false), &[2]).is_empty());
        assert!(fail_fast_cancellations(true, &leg(JobResult::Failure, true), &[2]).is_empty());
    }

    #[test]
    fn skipped_need_skips_dependent() {
        let src = "on: push\njobs:\n  a:\n    if: github.event_name == 'release'\n    runs-on: x\n    steps: [{run: x}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: x}]\n  c:\n    needs: a\n    if: ${{ !failure() && !cancelled() }}\n    runs-on: x\n    steps: [{run: x}]\n";
        let mut base = Object::new();
        base.insert(
            "github",
            Value::object([("event_name", Value::from("push"))]),
        );
        let mut g = RunGraph::new(&jobs(src), base).unwrap();
        let log = g.simulate(|_, _| ok()).unwrap();
        assert_eq!(log[0].1.result, JobResult::Skipped);
        assert_eq!(log[1].1.result, JobResult::Skipped);
        // The documented "run even if a dependency was skipped" idiom.
        assert_eq!(log[2].1.result, JobResult::Success);
        assert_eq!(g.conclusion(), Some(JobResult::Success));
    }
}
