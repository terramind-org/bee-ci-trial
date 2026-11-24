//! The deterministic plan: what the runner service executes for an event.
//!
//! Given a repository snapshot, an [`ActionsEvent`] and run facts, [`plan_event`] selects the
//! workflows that trigger and, for each, lists its jobs in dependency order with matrix legs
//! expanded, runner labels, containers and services, timeouts, environments, concurrency, and
//! every step's command or action with its environment.
//!
//! Everything that can be known before a runner starts is resolved: expressions over `github`,
//! `inputs`, `vars`, `matrix`, `strategy` (and `needs`, when the needed jobs' outcomes are
//! supplied). Expressions over runtime contexts (`steps`, `env`, `job`, `runner`, `secrets`,
//! status functions, `hashFiles`, and `github` properties only a runner knows, such as
//! `github.workspace`) stay as `${{ }}` parts for the runner. Substituted values are data:
//! the plan keeps template parts separate, so a value that happens to contain `${{` is never
//! evaluated again.
//!
//! A job whose pre-run settings (matrix, `runs-on`, name, ...) depend on `needs` outputs is
//! planned as [`JobBody::Deferred`]; call [`plan_job`] again with the outcomes once its needs
//! finish. A job's `if` is decided here only when it has no needs.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::error::{Diagnostic, ErrorKind, WorkflowError};
use crate::event::{
    ActionsEvent, RepoInfo, Selection, Triggered, dispatch_inputs, select_workflows,
};
use crate::expr::{Condition, ContextProvider, Expr, ExprError, Part, Status, Template, is_static};
use crate::graph::{JobOutcome, topo_order};
use crate::matrix::{self, MatrixError};
use crate::model::*;
use crate::parse::parse_workflow;
use crate::value::{Object, Value, format_number};
use crate::yaml::Pos;

/// GitHub's default job timeout, in minutes.
pub const DEFAULT_TIMEOUT_MINUTES: f64 = 360.0;
/// Workflow levels that can be connected through reusable workflows (the top-level caller
/// counts as one).
pub const MAX_WORKFLOW_LEVELS: usize = 10;
/// Unique reusable workflows one run may call.
pub const MAX_REUSABLE_WORKFLOWS: usize = 50;

/// Resolves reusable workflows in other repositories.
pub trait WorkflowResolver {
    /// The source of `owner/repo/path@git_ref`, if available.
    fn resolve(&self, owner: &str, repo: &str, path: &str, git_ref: &str) -> Option<String>;
}

/// Facts about the run being planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunInfo {
    /// `github.run_id`.
    pub run_id: u64,
    /// `github.run_number`.
    pub run_number: u64,
    /// `github.run_attempt`.
    pub run_attempt: u32,
}

/// Everything planning needs.
pub struct PlanInput<'a> {
    /// The repository.
    pub repo: &'a RepoInfo,
    /// The snapshot at `GITHUB_SHA`: path to file contents.
    pub files: &'a BTreeMap<String, String>,
    /// The event.
    pub event: &'a ActionsEvent,
    /// Run facts.
    pub run: RunInfo,
    /// The `vars` context (configuration variables).
    pub vars: Object,
    /// Resolver for reusable workflows in other repositories.
    pub resolver: Option<&'a dyn WorkflowResolver>,
}

/// A resolved concurrency setting.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedConcurrency {
    /// The group, as written after evaluation (compare case-insensitively).
    pub group: String,
    /// `cancel-in-progress`.
    pub cancel_in_progress: bool,
    /// `queue: max`.
    pub queue_max: bool,
}

/// A resolved `runs-on`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunsOn {
    /// Runner group, if given.
    pub group: Option<String>,
    /// Labels a runner must all have.
    pub labels: Vec<String>,
}

/// A resolved container or service.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedContainer {
    /// Image.
    pub image: String,
    /// `credentials` (`username`, `password`), usually secrets left for the runner.
    pub credentials: Vec<(String, Template)>,
    /// `env`.
    pub env: Vec<(String, Template)>,
    /// `ports`.
    pub ports: Vec<String>,
    /// `volumes`.
    pub volumes: Vec<String>,
    /// `options`.
    pub options: Option<String>,
}

/// A resolved deployment environment.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedEnvironment {
    /// Environment name.
    pub name: String,
    /// `url`, evaluated by the runner after the job.
    pub url: Option<Template>,
}

/// The shell a `run` step uses.
#[derive(Clone, Debug, PartialEq)]
pub struct Shell {
    /// The shell keyword (`bash`, `pwsh`, ...), a custom template, or `default`.
    pub name: String,
    /// The command template, `{0}` standing for the script file.
    pub command: String,
}

/// What a planned step does.
#[derive(Clone, Debug, PartialEq)]
pub enum PlannedStepKind {
    /// Runs a script.
    Run {
        /// The script.
        script: Template,
        /// The shell.
        shell: Shell,
        /// `working-directory`.
        working_directory: Option<Template>,
    },
    /// Runs an action.
    Uses {
        /// The action reference.
        action: String,
        /// `with` inputs.
        with: Vec<(String, Template)>,
    },
}

/// A planned step.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedStep {
    /// `id`.
    pub id: Option<String>,
    /// Display name.
    pub name: Template,
    /// `if` source, evaluated by the runner (implicitly `success()` when empty).
    pub condition: String,
    /// What it does.
    pub kind: PlannedStepKind,
    /// The step's own `env`.
    pub env: Vec<(String, Template)>,
    /// `continue-on-error` (a boolean or an expression for the runner).
    pub continue_on_error: Template,
    /// `timeout-minutes`, if set.
    pub timeout_minutes: Option<Template>,
}

/// The `strategy` context of a matrix leg.
#[derive(Clone, Debug, PartialEq)]
pub struct StrategyInfo {
    /// `strategy.job-index`.
    pub job_index: usize,
    /// `strategy.job-total`.
    pub job_total: usize,
    /// `strategy.fail-fast`.
    pub fail_fast: bool,
    /// `strategy.max-parallel` (the job total when unset).
    pub max_parallel: usize,
}

/// One runnable job (a matrix leg, or the only instance).
#[derive(Clone, Debug, PartialEq)]
pub struct JobInstance {
    /// Display name.
    pub name: String,
    /// The matrix combination.
    pub matrix: Option<Object>,
    /// Strategy facts.
    pub strategy: Option<StrategyInfo>,
    /// Where it runs.
    pub runs_on: RunsOn,
    /// Deployment environment.
    pub environment: Option<PlannedEnvironment>,
    /// Job-level concurrency.
    pub concurrency: Option<PlannedConcurrency>,
    /// Effective token permissions (`None`: the repository default).
    pub permissions: Option<Permissions>,
    /// Timeout in minutes.
    pub timeout_minutes: f64,
    /// `continue-on-error`.
    pub continue_on_error: bool,
    /// Job container.
    pub container: Option<PlannedContainer>,
    /// Service containers.
    pub services: Vec<(String, PlannedContainer)>,
    /// Environment from the workflow and job `env`, job values overriding.
    pub env: Vec<(String, Template)>,
    /// `outputs`.
    pub outputs: Vec<(String, Template)>,
    /// Steps.
    pub steps: Vec<PlannedStep>,
}

/// One call of a reusable workflow.
#[derive(Clone, Debug, PartialEq)]
pub struct CallInstance {
    /// Display name.
    pub name: String,
    /// The matrix combination of the calling job.
    pub matrix: Option<Object>,
    /// The `inputs` context of the called workflow.
    pub inputs: Object,
    /// Secrets passed: `inherit`, or `name: template` pairs.
    pub secrets: CallSecretsPlan,
    /// The called workflow's plan, or why it is not available yet.
    pub workflow: CalledWorkflow,
}

/// Secrets passed to a reusable workflow.
#[derive(Clone, Debug, PartialEq)]
pub enum CallSecretsPlan {
    /// None.
    None,
    /// `secrets: inherit`.
    Inherit,
    /// Explicit mapping, evaluated by the runner.
    Map(Vec<(String, Template)>),
}

/// A called workflow's plan.
#[derive(Clone, Debug, PartialEq)]
pub enum CalledWorkflow {
    /// Planned.
    Planned {
        /// The workflow's display name.
        name: String,
        /// Its workflow-level concurrency.
        concurrency: Option<PlannedConcurrency>,
        /// Its jobs.
        jobs: Vec<PlannedJob>,
    },
    /// In another repository with no resolver able to fetch it; the runner service resolves
    /// it before running.
    Unresolved(String),
}

/// What a planned job holds.
#[derive(Clone, Debug, PartialEq)]
pub enum JobBody {
    /// Ready: its instances (one per matrix leg).
    Instances(Vec<JobInstance>),
    /// Calls a reusable workflow, once per matrix leg.
    Call(Vec<CallInstance>),
    /// Its `if` is false: skipped.
    Skipped,
    /// Needs the outcomes of its `needs` before it can be expanded.
    Deferred(String),
}

/// A planned job.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedJob {
    /// Job id.
    pub id: String,
    /// `needs`.
    pub needs: Vec<String>,
    /// `if` source (empty for the default `success()`).
    pub condition: String,
    /// The decided value of `if`, when it could be decided at plan time.
    pub decided: Option<bool>,
    /// The body.
    pub body: JobBody,
}

/// A planned workflow run.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedRun {
    /// Workflow file path.
    pub workflow: String,
    /// Workflow display name.
    pub name: String,
    /// Run display name.
    pub run_name: String,
    /// `github.event_name`.
    pub event: String,
    /// `GITHUB_REF`.
    pub git_ref: String,
    /// `GITHUB_SHA`.
    pub sha: String,
    /// The matching cron entry, for `schedule`.
    pub schedule: Option<String>,
    /// The `inputs` context.
    pub inputs: Object,
    /// Workflow-level concurrency.
    pub concurrency: Option<PlannedConcurrency>,
    /// Workflow-level permissions.
    pub permissions: Option<Permissions>,
    /// Jobs in dependency order.
    pub jobs: Vec<PlannedJob>,
}

/// One workflow's entry in a plan.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanEntry {
    /// A run to start.
    Run(Box<PlannedRun>),
    /// The workflow is invalid or failed to plan; GitHub shows such runs as failed.
    Failed(WorkflowError),
}

/// The plan for one event.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Plan {
    /// Entries in workflow path order (then schedule order).
    pub entries: Vec<PlanEntry>,
}

struct PlanCtx {
    contexts: Object,
}

impl ContextProvider for PlanCtx {
    fn context(&self, name: &str) -> Option<Value> {
        self.contexts.get(name).cloned()
    }

    fn status(&self) -> Status {
        Status::SUCCESS
    }
}

/// Builds the `github` context known before a runner starts.
pub fn github_context(
    ev: &ActionsEvent,
    repo: &RepoInfo,
    run: &RunInfo,
    wf_path: &str,
    wf_name: &str,
) -> Value {
    let (ref_name, ref_type) = if let Some(b) = ev.git_ref.strip_prefix("refs/heads/") {
        (b.to_owned(), "branch")
    } else if let Some(t) = ev.git_ref.strip_prefix("refs/tags/") {
        (t.to_owned(), "tag")
    } else if let Some(p) = ev.git_ref.strip_prefix("refs/pull/") {
        (p.to_owned(), "branch")
    } else {
        (ev.git_ref.clone(), "")
    };
    Value::object([
        ("action_path", Value::Null),
        ("actor", Value::from(ev.actor.as_str())),
        ("api_url", Value::from(repo.api_url.as_str())),
        ("base_ref", Value::from(ev.base_ref.as_str())),
        ("event", ev.payload.clone()),
        ("event_name", Value::from(ev.name.as_str())),
        ("graphql_url", Value::from(repo.graphql_url.as_str())),
        ("head_ref", Value::from(ev.head_ref.as_str())),
        ("ref", Value::from(ev.git_ref.as_str())),
        ("ref_name", Value::from(ref_name)),
        ("ref_type", Value::from(ref_type)),
        ("repository", Value::from(repo.full_name())),
        ("repository_owner", Value::from(repo.owner.as_str())),
        ("run_attempt", Value::from(run.run_attempt.to_string())),
        ("run_id", Value::from(run.run_id.to_string())),
        ("run_number", Value::from(run.run_number.to_string())),
        ("server_url", Value::from(repo.server_url.as_str())),
        ("sha", Value::from(ev.sha.as_str())),
        ("triggering_actor", Value::from(ev.actor.as_str())),
        ("workflow", Value::from(wf_name)),
        (
            "workflow_ref",
            Value::from(format!("{}/{}@{}", repo.full_name(), wf_path, ev.git_ref)),
        ),
        ("workflow_sha", Value::from(ev.sha.as_str())),
    ])
}

/// Whether every `github.<prop>` use names a property the plan-time `github` context has.
fn github_static(e: &Expr, github: &Value) -> bool {
    match e {
        Expr::Context(c) => c != "github",
        Expr::Property(base, name) => match base.as_ref() {
            Expr::Context(c) if c == "github" => {
                github.get(name).is_some_and(|v| !matches!(v, Value::Null))
            }
            _ => github_static(base, github),
        },
        Expr::Index(base, idx) => {
            let base_ok = match (base.as_ref(), idx.as_ref()) {
                (Expr::Context(c), Expr::Literal(Value::String(s))) if c == "github" => {
                    github.get(s).is_some_and(|v| !matches!(v, Value::Null))
                }
                _ => github_static(base, github),
            };
            base_ok && github_static(idx, github)
        }
        Expr::Literal(_) => true,
        Expr::Wildcard(b) | Expr::Not(b) => github_static(b, github),
        Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            github_static(a, github) && github_static(b, github)
        }
        Expr::Call(_, args) => args.iter().all(|a| github_static(a, github)),
    }
}

struct Planner<'a> {
    input: &'a PlanInput<'a>,
    github: Value,
    called: Vec<WorkflowRef>,
}

#[derive(Clone)]
struct Scope {
    path: String,
    contexts: Object,
    known: Vec<&'static str>,
    depth: usize,
}

impl Scope {
    fn ctx(&self) -> PlanCtx {
        PlanCtx {
            contexts: self.contexts.clone(),
        }
    }

    fn with(&self, name: &'static str, v: Value) -> Scope {
        let mut s = self.clone();
        s.contexts.insert(name, v);
        if !s.known.contains(&name) {
            s.known.push(name);
        }
        s
    }
}

type PResult<T> = Result<T, WorkflowError>;

/// Raised when a pre-run setting needs `needs` outcomes that are not available yet.
const DEFER: &str = "\u{0}defer";

impl Planner<'_> {
    fn err(&self, scope: &Scope, pos: Pos, message: impl Into<String>) -> WorkflowError {
        WorkflowError::invalid(&scope.path, pos, message)
    }

    fn expr_err(&self, scope: &Scope, pos: Pos, what: &str, e: ExprError) -> WorkflowError {
        self.err(
            scope,
            pos,
            format!("error evaluating '{what}': {}", e.message),
        )
    }

    fn is_known(&self, scope: &Scope, e: &Expr) -> bool {
        is_static(e, &scope.known) && github_static(e, &self.github)
    }

    /// Evaluates a value that must be fully known now. Fails with `DEFER` when it reads
    /// `needs` that is not available.
    fn eval_now(&self, scope: &Scope, d: &Dyn, what: &str) -> PResult<Value> {
        let mut unknown: Option<Vec<String>> = None;
        d.for_each_template(&mut |t| {
            for e in t.exprs() {
                if !self.is_known(scope, e) && unknown.is_none() {
                    unknown = Some(e.contexts());
                }
            }
        });
        if let Some(ctxs) = unknown {
            if ctxs.iter().any(|c| c == "needs") && !scope.known.contains(&"needs") {
                return Err(self.err(scope, d.pos(), DEFER));
            }
            return Err(self.err(
                scope,
                d.pos(),
                format!("'{what}' uses a value that is not available before the job runs"),
            ));
        }
        d.evaluate(&scope.ctx())
            .map_err(|e| self.expr_err(scope, d.pos(), what, e))
    }

    fn template_now(&self, scope: &Scope, t: &Template, pos: Pos, what: &str) -> PResult<String> {
        self.eval_now(scope, &Dyn::Str(t.clone(), pos), what)
            .map(|v| v.to_display_string())
    }

    /// Partially evaluates a template: known expressions are substituted.
    fn partial(&self, scope: &Scope, t: &Template, pos: Pos, what: &str) -> PResult<Template> {
        let ctx = scope.ctx();
        let lone = t.parts.len() == 1;
        let mut parts: Vec<Part> = Vec::new();
        for p in &t.parts {
            match p {
                Part::Expr { expr, .. } if self.is_known(scope, expr) => {
                    let v = crate::expr::evaluate(expr, &ctx)
                        .map_err(|e| self.expr_err(scope, pos, what, e))?;
                    let text = match &v {
                        Value::Array(_) | Value::Object(_) if lone => v.to_json_compact(),
                        other => other.to_display_string(),
                    };
                    if let Some(Part::Text(prev)) = parts.last_mut() {
                        prev.push_str(&text);
                    } else if !text.is_empty() {
                        parts.push(Part::Text(text));
                    }
                }
                Part::Text(s) => {
                    if let Some(Part::Text(prev)) = parts.last_mut() {
                        prev.push_str(s);
                    } else {
                        parts.push(Part::Text(s.clone()));
                    }
                }
                other => parts.push(other.clone()),
            }
        }
        Ok(Template { parts })
    }

    fn partial_dyn(&self, scope: &Scope, d: &Dyn, what: &str) -> PResult<Template> {
        Ok(match d {
            Dyn::Null(_) => Template::default(),
            Dyn::Bool(b, _) => Template::literal(&b.to_string()),
            Dyn::Number(n, _) => Template::literal(&format_number(*n)),
            Dyn::Str(t, p) => self.partial(scope, t, *p, what)?,
            Dyn::Seq(..) | Dyn::Map(..) => {
                let v = self.eval_now(scope, d, what)?;
                Template::literal(&v.to_json_compact())
            }
        })
    }

    fn partial_env(&self, scope: &Scope, env: &EnvMap) -> PResult<Vec<(String, Template)>> {
        env.iter()
            .map(|(k, v)| Ok((k.clone(), self.partial_dyn(scope, v, &format!("env.{k}"))?)))
            .collect()
    }

    fn concurrency(&self, scope: &Scope, c: &Concurrency) -> PResult<PlannedConcurrency> {
        let group = self.template_now(scope, &c.group, c.pos, "concurrency.group")?;
        if group.is_empty() {
            return Err(self.err(
                scope,
                c.pos,
                "concurrency group evaluated to an empty string",
            ));
        }
        let cancel = self
            .eval_now(scope, &c.cancel_in_progress, "cancel-in-progress")?
            .is_truthy();
        Ok(PlannedConcurrency {
            group,
            cancel_in_progress: cancel,
            queue_max: c.queue_max,
        })
    }

    fn plan_jobs(
        &mut self,
        wf: &Workflow,
        scope: &Scope,
        needs: Option<&BTreeMap<String, JobOutcome>>,
    ) -> PResult<Vec<PlannedJob>> {
        let order = topo_order(&wf.jobs).map_err(|c| {
            self.err(
                scope,
                Pos::default(),
                format!("dependency cycle: {}", c.join(" -> ")),
            )
        })?;
        let mut out = Vec::new();
        for id in order {
            let Some(job) = wf.job(&id) else { continue };
            out.push(self.plan_one(wf, job, scope, needs)?);
        }
        Ok(out)
    }

    fn plan_one(
        &mut self,
        wf: &Workflow,
        job: &Job,
        scope: &Scope,
        needs: Option<&BTreeMap<String, JobOutcome>>,
    ) -> PResult<PlannedJob> {
        let condition = job
            .condition
            .as_ref()
            .map(|c| c.source.trim().to_owned())
            .unwrap_or_default();
        let mut scope = scope.clone();
        let needs_known = job.needs.is_empty()
            || needs.is_some_and(|n| {
                job.needs
                    .iter()
                    .all(|x| n.keys().any(|k| k.eq_ignore_ascii_case(x)))
            });
        if needs_known {
            let mut o = Object::new();
            if let Some(n) = needs {
                for x in &job.needs {
                    if let Some((k, out)) = n.iter().find(|(k, _)| k.eq_ignore_ascii_case(x)) {
                        o.insert(
                            k.clone(),
                            Value::object([
                                ("result", Value::from(out.result.as_str())),
                                ("outputs", Value::from(out.outputs.clone())),
                            ]),
                        );
                    }
                }
            }
            scope = scope.with("needs", Value::from(o));
        }
        let decided = if job.needs.is_empty() {
            let c = job
                .condition
                .clone()
                .unwrap_or_else(Condition::default_success);
            let all_known = c.exprs().iter().all(|e| {
                is_static(&strip_status(e), &scope.known) && github_static(e, &self.github)
            });
            if all_known {
                Some(
                    c.evaluate(&scope.ctx())
                        .map_err(|e| self.expr_err(&scope, job.pos, "if", e))?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let body = if decided == Some(false) {
            JobBody::Skipped
        } else {
            match self.expand(wf, job, &scope) {
                Ok(b) => b,
                Err(e) if e.first().is_some_and(|d| d.message == DEFER) => JobBody::Deferred(
                    format!("waits for the outcomes of: {}", job.needs.join(", ")),
                ),
                Err(e) => return Err(e),
            }
        };
        Ok(PlannedJob {
            id: job.id.clone(),
            needs: job.needs.clone(),
            condition,
            decided,
            body,
        })
    }

    fn strategy(
        &self,
        job: &Job,
        scope: &Scope,
    ) -> PResult<(Vec<Option<Object>>, bool, Option<usize>)> {
        let Some(s) = &job.strategy else {
            return Ok((vec![None], true, None));
        };
        let fail_fast = match &s.fail_fast {
            Some(d) => self.eval_now(scope, d, "fail-fast")?.is_truthy(),
            None => true,
        };
        let max_parallel = match &s.max_parallel {
            Some(d) => {
                let n = self.eval_now(scope, d, "max-parallel")?.to_number();
                if !n.is_finite() || n < 1.0 {
                    return Err(self.err(scope, d.pos(), "max-parallel must be a positive number"));
                }
                Some(n.floor() as usize)
            }
            None => None,
        };
        let combos = match &s.matrix {
            None => vec![None],
            Some(m) => {
                let v = self.eval_now(scope, m, "strategy.matrix")?;
                match matrix::expand(&v) {
                    Ok(c) if c.is_empty() => {
                        return Err(self.err(
                            scope,
                            m.pos(),
                            "the strategy matrix produced no job combinations",
                        ));
                    }
                    Ok(c) => c.into_iter().map(Some).collect(),
                    Err(MatrixError::TooLarge(n)) => {
                        return Err(WorkflowError {
                            path: scope.path.clone(),
                            kind: ErrorKind::TooLarge {
                                limit: "matrix_jobs".to_owned(),
                                max: matrix::MAX_MATRIX_JOBS as u64,
                                actual: n,
                            },
                            diagnostics: vec![Diagnostic {
                                pos: m.pos(),
                                message: MatrixError::TooLarge(n).to_string(),
                            }],
                        });
                    }
                    Err(e) => return Err(self.err(scope, m.pos(), e.to_string())),
                }
            }
        };
        Ok((combos, fail_fast, max_parallel))
    }

    fn leg_scope(
        scope: &Scope,
        combo: &Option<Object>,
        index: usize,
        total: usize,
        fail_fast: bool,
        max_parallel: Option<usize>,
    ) -> (Scope, Option<StrategyInfo>) {
        let info = StrategyInfo {
            job_index: index,
            job_total: total,
            fail_fast,
            max_parallel: max_parallel.unwrap_or(total),
        };
        let strategy = Value::object([
            ("fail-fast", Value::Bool(fail_fast)),
            ("job-index", Value::Number(index as f64)),
            ("job-total", Value::Number(total as f64)),
            ("max-parallel", Value::Number(info.max_parallel as f64)),
        ]);
        let matrix = combo
            .clone()
            .map(Value::from)
            .unwrap_or_else(|| Value::from(Object::new()));
        let s = scope.with("matrix", matrix).with("strategy", strategy);
        (s, combo.as_ref().map(|_| info))
    }

    fn display_name(&self, job: &Job, scope: &Scope, combo: &Option<Object>) -> PResult<String> {
        let (base, has_expr) = match &job.name {
            Some(t) => (
                self.template_now(scope, t, job.pos, "name")?,
                !t.is_literal(),
            ),
            None => (job.id.clone(), false),
        };
        Ok(match combo {
            Some(c) if !has_expr && !c.is_empty() => {
                format!("{base} ({})", matrix::display_values(c))
            }
            _ => base,
        })
    }

    fn expand(&mut self, wf: &Workflow, job: &Job, scope: &Scope) -> PResult<JobBody> {
        let (combos, fail_fast, max_parallel) = self.strategy(job, scope)?;
        let total = combos.len();
        match &job.kind {
            JobKind::Steps(s) => {
                let mut legs = Vec::new();
                for (i, combo) in combos.iter().enumerate() {
                    let (ls, info) =
                        Self::leg_scope(scope, combo, i, total, fail_fast, max_parallel);
                    legs.push(self.instance(wf, job, s, &ls, combo, info)?);
                }
                Ok(JobBody::Instances(legs))
            }
            JobKind::Call(c) => {
                let mut calls = Vec::new();
                for (i, combo) in combos.iter().enumerate() {
                    let (ls, _) = Self::leg_scope(scope, combo, i, total, fail_fast, max_parallel);
                    calls.push(self.call(job, c, &ls, combo)?);
                }
                Ok(JobBody::Call(calls))
            }
        }
    }

    fn runs_on(&self, scope: &Scope, d: &Dyn) -> PResult<RunsOn> {
        let v = self.eval_now(scope, d, "runs-on")?;
        let labels_of = |v: &Value| -> Option<Vec<String>> {
            match v {
                Value::String(s) => Some(vec![s.clone()]),
                Value::Array(a) => a
                    .iter()
                    .map(|x| match x {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(_) | Value::Bool(_) => Some(x.to_display_string()),
                        _ => None,
                    })
                    .collect(),
                _ => None,
            }
        };
        let bad = || {
            self.err(scope, d.pos(), "'runs-on' must be a label, a list of labels, or a mapping with 'group' and 'labels'")
        };
        let r = match &v {
            Value::Object(o) => RunsOn {
                group: o
                    .get("group")
                    .map(Value::to_display_string)
                    .filter(|g| !g.is_empty()),
                labels: match o.get("labels") {
                    Some(l) => labels_of(l).ok_or_else(bad)?,
                    None => Vec::new(),
                },
            },
            other => RunsOn {
                group: None,
                labels: labels_of(other).ok_or_else(bad)?,
            },
        };
        if r.group.is_none() && (r.labels.is_empty() || r.labels.iter().all(String::is_empty)) {
            return Err(self.err(scope, d.pos(), "'runs-on' evaluated to no runner labels"));
        }
        Ok(r)
    }

    fn container(&self, scope: &Scope, c: &Container) -> PResult<Option<PlannedContainer>> {
        let image = self.eval_now(scope, &c.image, "image")?.to_display_string();
        if image.is_empty() {
            return Ok(None);
        }
        let credentials = match &c.credentials {
            Some(Dyn::Map(entries, _)) => entries
                .iter()
                .map(|(k, v)| Ok((k.clone(), self.partial_dyn(scope, v, "credentials")?)))
                .collect::<PResult<Vec<_>>>()?,
            Some(other) => vec![(
                "value".to_owned(),
                self.partial_dyn(scope, other, "credentials")?,
            )],
            None => Vec::new(),
        };
        let list = |items: &[Dyn], what: &str| -> PResult<Vec<String>> {
            items
                .iter()
                .map(|d| Ok(self.eval_now(scope, d, what)?.to_display_string()))
                .collect()
        };
        Ok(Some(PlannedContainer {
            image,
            credentials,
            env: self.partial_env(scope, &c.env)?,
            ports: list(&c.ports, "ports")?,
            volumes: list(&c.volumes, "volumes")?,
            options: match &c.options {
                Some(o) => Some(self.eval_now(scope, o, "options")?.to_display_string()),
                None => None,
            },
        }))
    }

    fn instance(
        &self,
        wf: &Workflow,
        job: &Job,
        s: &StepsJob,
        scope: &Scope,
        combo: &Option<Object>,
        strategy: Option<StrategyInfo>,
    ) -> PResult<JobInstance> {
        let name = self.display_name(job, scope, combo)?;
        let runs_on = self.runs_on(scope, &s.runs_on)?;
        let environment = match &s.environment {
            None => None,
            Some(Dyn::Map(entries, _)) => {
                let mut name = String::new();
                let mut url = None;
                for (k, v) in entries {
                    match k.as_str() {
                        "name" => {
                            name = self
                                .eval_now(scope, v, "environment.name")?
                                .to_display_string()
                        }
                        "url" => url = Some(self.partial_dyn(scope, v, "environment.url")?),
                        _ => {}
                    }
                }
                if name.is_empty() {
                    None
                } else {
                    Some(PlannedEnvironment { name, url })
                }
            }
            Some(d) => {
                let name = self.eval_now(scope, d, "environment")?.to_display_string();
                if name.is_empty() {
                    None
                } else {
                    Some(PlannedEnvironment { name, url: None })
                }
            }
        };
        let concurrency = match &job.concurrency {
            Some(c) => Some(self.concurrency(scope, c)?),
            None => None,
        };
        let timeout_minutes = match &s.timeout_minutes {
            Some(d) => {
                let n = self.eval_now(scope, d, "timeout-minutes")?.to_number();
                if !n.is_finite() || n <= 0.0 {
                    return Err(self.err(
                        scope,
                        d.pos(),
                        "timeout-minutes must be a positive number",
                    ));
                }
                n
            }
            None => DEFAULT_TIMEOUT_MINUTES,
        };
        let continue_on_error = match &s.continue_on_error {
            Some(d) => self.eval_now(scope, d, "continue-on-error")?.is_truthy(),
            None => false,
        };
        let container = match &s.container {
            Some(c) => self.container(scope, c)?,
            None => None,
        };
        let mut services = Vec::new();
        for (id, c) in &s.services {
            if let Some(pc) = self.container(scope, c)? {
                services.push((id.clone(), pc));
            }
        }
        // Workflow env (with workflow contexts) then job env overriding.
        let mut env = self.partial_env(scope, &wf.env)?;
        for (k, v) in self.partial_env(scope, &s.env)? {
            if let Some(slot) = env.iter_mut().find(|(ek, _)| *ek == k) {
                slot.1 = v;
            } else {
                env.push((k, v));
            }
        }
        let outputs = s
            .outputs
            .iter()
            .map(|(k, t)| Ok((k.clone(), self.partial(scope, t, job.pos, "outputs")?)))
            .collect::<PResult<Vec<_>>>()?;
        let windows = runs_on
            .labels
            .iter()
            .any(|l| l.to_ascii_lowercase().starts_with("windows"));
        let mut steps = Vec::new();
        for step in &s.steps {
            steps.push(self.step(wf, s, scope, step, windows)?);
        }
        Ok(JobInstance {
            name,
            matrix: combo.clone(),
            strategy,
            runs_on,
            environment,
            concurrency,
            permissions: job.permissions.clone().or_else(|| wf.permissions.clone()),
            timeout_minutes,
            continue_on_error,
            container,
            services,
            env,
            outputs,
            steps,
        })
    }

    fn shell(&self, scope: &Scope, name: Option<&Dyn>, windows: bool, pos: Pos) -> PResult<Shell> {
        let Some(d) = name else {
            return Ok(if windows {
                Shell {
                    name: "pwsh".to_owned(),
                    command: shell_command("pwsh").unwrap_or_default().to_owned(),
                }
            } else {
                Shell {
                    name: "default".to_owned(),
                    command: "bash -e {0}".to_owned(),
                }
            });
        };
        let t = self.partial_dyn(scope, d, "shell")?;
        if !t.is_literal() {
            let r = t.render();
            return Ok(Shell {
                name: r.clone(),
                command: r,
            });
        }
        let n = t.render();
        match shell_command(&n) {
            Some(c) => Ok(Shell {
                name: n,
                command: c.to_owned(),
            }),
            None if n.contains("{0}") => Ok(Shell {
                name: n.clone(),
                command: n,
            }),
            None => Err(self.err(
                scope,
                pos,
                format!("Invalid shell option '{n}': a custom shell must contain '{{0}}'"),
            )),
        }
    }

    fn step(
        &self,
        wf: &Workflow,
        s: &StepsJob,
        scope: &Scope,
        step: &Step,
        windows: bool,
    ) -> PResult<PlannedStep> {
        let kind = match &step.action {
            StepAction::Run {
                script,
                shell,
                working_directory,
            } => {
                let shell_dyn = shell
                    .as_ref()
                    .or(s.defaults.shell.as_ref())
                    .or(wf.defaults.shell.as_ref());
                let wd = working_directory
                    .as_ref()
                    .or(s.defaults.working_directory.as_ref())
                    .or(wf.defaults.working_directory.as_ref());
                PlannedStepKind::Run {
                    script: self.partial(scope, script, step.pos, "run")?,
                    shell: self.shell(scope, shell_dyn, windows, step.pos)?,
                    working_directory: match wd {
                        Some(d) => Some(self.partial_dyn(scope, d, "working-directory")?),
                        None => None,
                    },
                }
            }
            StepAction::Uses { action, with } => PlannedStepKind::Uses {
                action: action.render(),
                with: with
                    .iter()
                    .map(|(k, v)| {
                        Ok((k.clone(), self.partial_dyn(scope, v, &format!("with.{k}"))?))
                    })
                    .collect::<PResult<Vec<_>>>()?,
            },
        };
        let name = match (&step.name, &kind) {
            (Some(t), _) => self.partial(scope, t, step.pos, "name")?,
            // The default name shows the script as it will run.
            (None, PlannedStepKind::Run { script, .. }) => {
                let first = script
                    .render()
                    .trim_start()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim_end()
                    .to_owned();
                Template::literal(&format!("Run {first}"))
            }
            (None, _) => Template::literal(&default_step_name(&step.action)),
        };
        Ok(PlannedStep {
            id: step.id.clone(),
            name,
            condition: step
                .condition
                .as_ref()
                .map(|c| c.source.trim().to_owned())
                .unwrap_or_default(),
            kind,
            env: self.partial_env(scope, &step.env)?,
            continue_on_error: match &step.continue_on_error {
                Some(d) => self.partial_dyn(scope, d, "continue-on-error")?,
                None => Template::literal("false"),
            },
            timeout_minutes: match &step.timeout_minutes {
                Some(d) => Some(self.partial_dyn(scope, d, "timeout-minutes")?),
                None => None,
            },
        })
    }

    fn call(
        &mut self,
        job: &Job,
        c: &CallJob,
        scope: &Scope,
        combo: &Option<Object>,
    ) -> PResult<CallInstance> {
        let name = self.display_name(job, scope, combo)?;
        let mut given = Object::new();
        for (k, v) in &c.with {
            given.insert(k.clone(), self.eval_now(scope, v, &format!("with.{k}"))?);
        }
        let secrets = match &c.secrets {
            CallSecrets::None => CallSecretsPlan::None,
            CallSecrets::Inherit => CallSecretsPlan::Inherit,
            CallSecrets::Map(m) => CallSecretsPlan::Map(
                m.iter()
                    .map(|(k, v)| {
                        Ok((
                            k.clone(),
                            self.partial_dyn(scope, v, &format!("secrets.{k}"))?,
                        ))
                    })
                    .collect::<PResult<Vec<_>>>()?,
            ),
        };
        if scope.depth + 1 >= MAX_WORKFLOW_LEVELS {
            return Err(self.err(
                scope,
                job.pos,
                format!(
                    "reusable workflows can be nested at most {MAX_WORKFLOW_LEVELS} levels deep"
                ),
            ));
        }
        if !self.called.contains(&c.uses) {
            self.called.push(c.uses.clone());
            if self.called.len() > MAX_REUSABLE_WORKFLOWS {
                return Err(self.err(
                    scope,
                    job.pos,
                    format!(
                        "a run can call at most {MAX_REUSABLE_WORKFLOWS} unique reusable workflows"
                    ),
                ));
            }
        }
        let (callee_path, source) = match &c.uses {
            WorkflowRef::Local(p) => match self.input.files.get(p) {
                Some(src) => (p.clone(), src.clone()),
                None => {
                    return Err(self.err(
                        scope,
                        job.pos,
                        format!("reusable workflow '{}' was not found", c.uses.render()),
                    ));
                }
            },
            WorkflowRef::Remote {
                owner,
                repo,
                path,
                git_ref,
            } => {
                match self
                    .input
                    .resolver
                    .and_then(|r| r.resolve(owner, repo, path, git_ref))
                {
                    Some(src) => (c.uses.render(), src),
                    None => {
                        return Ok(CallInstance {
                            name,
                            matrix: combo.clone(),
                            inputs: given,
                            secrets,
                            workflow: CalledWorkflow::Unresolved(c.uses.render()),
                        });
                    }
                }
            }
        };
        let callee = parse_workflow(&callee_path, &source)?;
        let Some(trigger) = callee.trigger("workflow_call") else {
            return Err(self.err(
                scope,
                job.pos,
                format!(
                    "workflow is not reusable as it is missing a 'on.workflow_call' trigger: {}",
                    c.uses.render()
                ),
            ));
        };
        // Inputs.
        for (k, _) in given.iter() {
            if !trigger.inputs.iter().any(|d| d.id.eq_ignore_ascii_case(k)) {
                return Err(self.err(
                    scope,
                    job.pos,
                    format!("Invalid input, {k} is not defined in the referenced workflow."),
                ));
            }
        }
        let mut callee_scope = Scope {
            path: callee_path.clone(),
            contexts: Object::new(),
            known: vec!["github", "inputs", "vars"],
            depth: scope.depth + 1,
        };
        callee_scope.contexts.insert("github", self.github.clone());
        callee_scope
            .contexts
            .insert("vars", Value::from(self.input.vars.clone()));
        callee_scope
            .contexts
            .insert("inputs", Value::from(Object::new()));
        let mut inputs = Object::new();
        for d in &trigger.inputs {
            let v = match given.get(&d.id) {
                Some(v) => v.clone(),
                None => match &d.default {
                    Some(def) => {
                        self.eval_now(&callee_scope, def, &format!("inputs.{}.default", d.id))?
                    }
                    None if d.required => {
                        return Err(self.err(
                            scope,
                            job.pos,
                            format!(
                                "Input {} is required, but not provided while calling.",
                                d.id
                            ),
                        ));
                    }
                    None => match d.kind {
                        InputType::Boolean => Value::Bool(false),
                        InputType::Number => Value::Number(0.0),
                        _ => Value::from(""),
                    },
                },
            };
            let v = match (d.kind, &v) {
                (InputType::Boolean, Value::Bool(_)) | (InputType::Number, Value::Number(_)) => {
                    v.clone()
                }
                (InputType::String, Value::String(_)) => v.clone(),
                (InputType::String, Value::Number(_) | Value::Bool(_)) => {
                    Value::from(v.to_display_string())
                }
                (_, Value::Null) if !d.required => Value::Null,
                _ => {
                    return Err(self.err(
                        scope,
                        job.pos,
                        format!(
                            "Input {} must be a {}, got '{}'",
                            d.id,
                            d.kind.name(),
                            v.to_display_string()
                        ),
                    ));
                }
            };
            inputs.insert(d.id.clone(), v);
        }
        // Secrets.
        match &secrets {
            CallSecretsPlan::Inherit => {}
            CallSecretsPlan::None | CallSecretsPlan::Map(_) => {
                let passed: Vec<&str> = match &secrets {
                    CallSecretsPlan::Map(m) => m.iter().map(|(k, _)| k.as_str()).collect(),
                    _ => Vec::new(),
                };
                for k in &passed {
                    if !trigger.secrets.iter().any(|d| d.id.eq_ignore_ascii_case(k)) {
                        return Err(self.err(
                            scope,
                            job.pos,
                            format!(
                                "Invalid secret, {k} is not defined in the referenced workflow."
                            ),
                        ));
                    }
                }
                for d in &trigger.secrets {
                    if d.required && !passed.iter().any(|p| p.eq_ignore_ascii_case(&d.id)) {
                        return Err(self.err(
                            scope,
                            job.pos,
                            format!(
                                "Secret {} is required, but not provided while calling.",
                                d.id
                            ),
                        ));
                    }
                }
            }
        }
        callee_scope
            .contexts
            .insert("inputs", Value::from(inputs.clone()));
        let concurrency = match &callee.concurrency {
            Some(cc) => Some(self.concurrency(&callee_scope, cc)?),
            None => None,
        };
        let jobs = self.plan_jobs(&callee, &callee_scope, None)?;
        Ok(CallInstance {
            name,
            matrix: combo.clone(),
            inputs,
            secrets,
            workflow: CalledWorkflow::Planned {
                name: callee.display_name().to_owned(),
                concurrency,
                jobs,
            },
        })
    }
}

/// Replaces status-function calls so static analysis ignores them (their value is known at
/// plan time for jobs without needs).
fn strip_status(e: &Expr) -> Expr {
    match e {
        Expr::Call(f, _) if f.is_status() => Expr::Literal(Value::Bool(true)),
        Expr::Call(f, args) => Expr::Call(*f, args.iter().map(strip_status).collect()),
        Expr::Property(b, n) => Expr::Property(Box::new(strip_status(b)), n.clone()),
        Expr::Index(a, b) => Expr::Index(Box::new(strip_status(a)), Box::new(strip_status(b))),
        Expr::Wildcard(b) => Expr::Wildcard(Box::new(strip_status(b))),
        Expr::Not(b) => Expr::Not(Box::new(strip_status(b))),
        Expr::Cmp(op, a, b) => Expr::Cmp(*op, Box::new(strip_status(a)), Box::new(strip_status(b))),
        Expr::And(a, b) => Expr::And(Box::new(strip_status(a)), Box::new(strip_status(b))),
        Expr::Or(a, b) => Expr::Or(Box::new(strip_status(a)), Box::new(strip_status(b))),
        other => other.clone(),
    }
}

/// The command template for a known shell keyword (GitHub's shell table).
pub fn shell_command(name: &str) -> Option<&'static str> {
    Some(match name {
        "bash" => "bash --noprofile --norc -eo pipefail {0}",
        "sh" => "sh -e {0}",
        "pwsh" => "pwsh -command \". '{0}'\"",
        "python" => "python {0}",
        "cmd" => "%ComSpec% /D /E:ON /V:OFF /S /C \"CALL \"{0}\"\"",
        "powershell" => "powershell -command \". '{0}'\"",
        _ => return None,
    })
}

/// A step's default display name: `Run <first script line>` or `Run <action>`.
pub fn default_step_name(action: &StepAction) -> String {
    match action {
        StepAction::Run { script, .. } => {
            let text = script.render();
            let first = text
                .trim_start()
                .lines()
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_owned();
            format!("Run {first}")
        }
        StepAction::Uses { action, .. } => format!("Run {}", action.render()),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

fn default_run_name(wf: &Workflow, ev: &ActionsEvent) -> String {
    let from_payload = match ev.name.as_str() {
        "push" => ev
            .payload
            .get("head_commit")
            .and_then(|h| h.get("message"))
            .and_then(Value::as_str)
            .map(|m| first_line(m).to_owned()),
        "pull_request" | "pull_request_target" | "pull_request_review" => ev
            .payload
            .get("pull_request")
            .and_then(|p| p.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    };
    from_payload
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| wf.display_name().to_owned())
}

struct Base {
    scope: Scope,
    github: Value,
    inputs: Object,
    event: ActionsEvent,
}

fn base_scope(
    wf: &Workflow,
    schedule: Option<&str>,
    input: &PlanInput<'_>,
) -> Result<Base, WorkflowError> {
    let ev = input.event;
    let (inputs, event_payload) = if ev.name == "workflow_dispatch" {
        let given = match &ev.subject {
            crate::event::FilterSubject::Dispatch(_, i) => i.clone(),
            _ => BTreeMap::new(),
        };
        let (typed, strings) = dispatch_inputs(wf, &given)
            .map_err(|m| WorkflowError::invalid(&wf.path, Pos::default(), m))?;
        let mut payload = ev.payload.as_object().cloned().unwrap_or_default();
        payload.insert("inputs", Value::from(strings));
        (typed, Value::from(payload))
    } else {
        (Object::new(), ev.payload.clone())
    };
    let mut event = ev.clone();
    event.payload = event_payload;
    if let Some(cron) = schedule {
        let mut p = event.payload.as_object().cloned().unwrap_or_default();
        p.insert("schedule", Value::from(cron));
        event.payload = Value::from(p);
    }
    let github = github_context(&event, input.repo, &input.run, &wf.path, wf.display_name());
    let mut contexts = Object::new();
    contexts.insert("github", github.clone());
    contexts.insert("inputs", Value::from(inputs.clone()));
    contexts.insert("vars", Value::from(input.vars.clone()));
    let scope = Scope {
        path: wf.path.clone(),
        contexts,
        known: vec!["github", "inputs", "vars"],
        depth: 0,
    };
    Ok(Base {
        scope,
        github,
        inputs,
        event,
    })
}

/// Plans one triggered workflow.
pub fn plan_workflow(
    wf: &Workflow,
    triggered: &Triggered,
    input: &PlanInput<'_>,
) -> Result<PlannedRun, WorkflowError> {
    let Base {
        scope,
        github,
        inputs,
        event,
    } = base_scope(wf, triggered.schedule.as_deref(), input)?;
    let mut planner = Planner {
        input,
        github,
        called: Vec::new(),
    };
    let run_name = match &wf.run_name {
        Some(t) => {
            let s = planner.template_now(&scope, t, Pos::default(), "run-name")?;
            if s.trim().is_empty() {
                default_run_name(wf, &event)
            } else {
                s
            }
        }
        None => default_run_name(wf, &event),
    };
    let concurrency = match &wf.concurrency {
        Some(c) => Some(planner.concurrency(&scope, c)?),
        None => None,
    };
    let jobs = planner.plan_jobs(wf, &scope, None)?;
    Ok(PlannedRun {
        workflow: wf.path.clone(),
        name: wf.display_name().to_owned(),
        run_name,
        event: event.name.clone(),
        git_ref: event.git_ref.clone(),
        sha: event.sha.clone(),
        schedule: triggered.schedule.clone(),
        inputs,
        concurrency,
        permissions: wf.permissions.clone(),
        jobs,
    })
}

/// Re-plans one job once the outcomes of its needs are known (for deferred jobs). Pass the
/// same `schedule` the run was triggered with, if any.
pub fn plan_job(
    wf: &Workflow,
    job_id: &str,
    schedule: Option<&str>,
    input: &PlanInput<'_>,
    needs: &BTreeMap<String, JobOutcome>,
) -> Result<PlannedJob, WorkflowError> {
    let Some(job) = wf.job(job_id) else {
        return Err(WorkflowError::invalid(
            &wf.path,
            Pos::default(),
            format!("unknown job '{job_id}'"),
        ));
    };
    let Base { scope, github, .. } = base_scope(wf, schedule, input)?;
    let mut planner = Planner {
        input,
        github,
        called: Vec::new(),
    };
    planner.plan_one(wf, job, &scope, Some(needs))
}

/// Plans every workflow an event triggers.
pub fn plan_event(input: &PlanInput<'_>) -> Plan {
    let mut plan = Plan::default();
    for (_, sel) in select_workflows(input.files, input.event) {
        match sel {
            Selection::Invalid(e) => plan.entries.push(PlanEntry::Failed(e)),
            Selection::Run(wf, triggers) => {
                for t in &triggers {
                    match plan_workflow(&wf, t, input) {
                        Ok(r) => plan.entries.push(PlanEntry::Run(Box::new(r))),
                        Err(e) => plan.entries.push(PlanEntry::Failed(e)),
                    }
                }
            }
        }
    }
    plan
}

// ---------------------------------------------------------------------------------------------
// Rendering

fn q(s: &str) -> String {
    Value::from(s).to_json_compact()
}

fn tq(t: &Template) -> String {
    q(&t.render())
}

fn render_perms(p: &Option<Permissions>) -> String {
    match p {
        None => "default".to_owned(),
        Some(Permissions::ReadAll) => "read-all".to_owned(),
        Some(Permissions::WriteAll) => "write-all".to_owned(),
        Some(Permissions::Scopes(s)) if s.is_empty() => "{}".to_owned(),
        Some(Permissions::Scopes(s)) => s
            .iter()
            .map(|(k, a)| format!("{k}={}", a.name()))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn render_conc(c: &PlannedConcurrency) -> String {
    format!(
        "{} cancel-in-progress={}{}",
        q(&c.group),
        c.cancel_in_progress,
        if c.queue_max { " queue=max" } else { "" }
    )
}

fn line(out: &mut String, indent: usize, text: &str) {
    for _ in 0..indent {
        out.push_str("  ");
    }
    out.push_str(text);
    out.push('\n');
}

fn render_pairs(out: &mut String, indent: usize, label: &str, pairs: &[(String, Template)]) {
    if pairs.is_empty() {
        return;
    }
    line(out, indent, &format!("{label}:"));
    for (k, v) in pairs {
        line(out, indent + 1, &format!("{k}: {}", tq(v)));
    }
}

fn render_container(out: &mut String, indent: usize, label: &str, c: &PlannedContainer) {
    line(out, indent, &format!("{label}: {}", q(&c.image)));
    render_pairs(out, indent + 1, "credentials", &c.credentials);
    render_pairs(out, indent + 1, "env", &c.env);
    if !c.ports.is_empty() {
        line(out, indent + 1, &format!("ports: {}", c.ports.join(", ")));
    }
    if !c.volumes.is_empty() {
        line(
            out,
            indent + 1,
            &format!("volumes: {}", c.volumes.join(", ")),
        );
    }
    if let Some(o) = &c.options {
        line(out, indent + 1, &format!("options: {}", q(o)));
    }
}

fn render_jobs(out: &mut String, indent: usize, jobs: &[PlannedJob]) {
    for j in jobs {
        let mut head = format!("job {}", j.id);
        if !j.needs.is_empty() {
            let _ = write!(head, " needs=[{}]", j.needs.join(", "));
        }
        if !j.condition.is_empty() {
            let _ = write!(head, " if={}", q(&j.condition));
        }
        if let Some(d) = j.decided {
            let _ = write!(head, " decided={d}");
        }
        line(out, indent, &head);
        match &j.body {
            JobBody::Skipped => line(out, indent + 1, "skipped"),
            JobBody::Deferred(r) => line(out, indent + 1, &format!("deferred: {r}")),
            JobBody::Instances(legs) => {
                for leg in legs {
                    line(out, indent + 1, &format!("instance {}", q(&leg.name)));
                    let i = indent + 2;
                    if let Some(m) = &leg.matrix {
                        line(
                            out,
                            i,
                            &format!("matrix: {}", Value::from(m.clone()).to_json_compact()),
                        );
                    }
                    if let Some(s) = &leg.strategy {
                        line(
                            out,
                            i,
                            &format!(
                                "strategy: index={} total={} fail-fast={} max-parallel={}",
                                s.job_index, s.job_total, s.fail_fast, s.max_parallel
                            ),
                        );
                    }
                    let mut ro = format!(
                        "runs-on: [{}]",
                        leg.runs_on
                            .labels
                            .iter()
                            .map(|l| q(l))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    if let Some(g) = &leg.runs_on.group {
                        let _ = write!(ro, " group={}", q(g));
                    }
                    line(out, i, &ro);
                    if let Some(e) = &leg.environment {
                        match &e.url {
                            Some(u) => line(
                                out,
                                i,
                                &format!("environment: {} url={}", q(&e.name), tq(u)),
                            ),
                            None => line(out, i, &format!("environment: {}", q(&e.name))),
                        }
                    }
                    if let Some(c) = &leg.concurrency {
                        line(out, i, &format!("concurrency: {}", render_conc(c)));
                    }
                    line(
                        out,
                        i,
                        &format!("permissions: {}", render_perms(&leg.permissions)),
                    );
                    line(
                        out,
                        i,
                        &format!("timeout-minutes: {}", format_number(leg.timeout_minutes)),
                    );
                    if leg.continue_on_error {
                        line(out, i, "continue-on-error: true");
                    }
                    if let Some(c) = &leg.container {
                        render_container(out, i, "container", c);
                    }
                    for (id, c) in &leg.services {
                        render_container(out, i, &format!("service {id}"), c);
                    }
                    render_pairs(out, i, "env", &leg.env);
                    render_pairs(out, i, "outputs", &leg.outputs);
                    for (n, s) in leg.steps.iter().enumerate() {
                        let mut h = format!("step {} {}", n + 1, tq(&s.name));
                        if let Some(id) = &s.id {
                            let _ = write!(h, " id={id}");
                        }
                        if !s.condition.is_empty() {
                            let _ = write!(h, " if={}", q(&s.condition));
                        }
                        line(out, i, &h);
                        let si = i + 1;
                        match &s.kind {
                            PlannedStepKind::Run {
                                script,
                                shell,
                                working_directory,
                            } => {
                                line(out, si, &format!("run: {}", tq(script)));
                                line(
                                    out,
                                    si,
                                    &format!("shell: {} -> {}", shell.name, q(&shell.command)),
                                );
                                if let Some(w) = working_directory {
                                    line(out, si, &format!("working-directory: {}", tq(w)));
                                }
                            }
                            PlannedStepKind::Uses { action, with } => {
                                line(out, si, &format!("uses: {action}"));
                                render_pairs(out, si, "with", with);
                            }
                        }
                        render_pairs(out, si, "env", &s.env);
                        if s.continue_on_error.render() != "false" {
                            line(
                                out,
                                si,
                                &format!("continue-on-error: {}", tq(&s.continue_on_error)),
                            );
                        }
                        if let Some(t) = &s.timeout_minutes {
                            line(out, si, &format!("timeout-minutes: {}", tq(t)));
                        }
                    }
                }
            }
            JobBody::Call(calls) => {
                for c in calls {
                    line(out, indent + 1, &format!("call {}", q(&c.name)));
                    let i = indent + 2;
                    if let Some(m) = &c.matrix {
                        line(
                            out,
                            i,
                            &format!("matrix: {}", Value::from(m.clone()).to_json_compact()),
                        );
                    }
                    line(
                        out,
                        i,
                        &format!(
                            "inputs: {}",
                            Value::from(c.inputs.clone()).to_json_compact()
                        ),
                    );
                    match &c.secrets {
                        CallSecretsPlan::None => {}
                        CallSecretsPlan::Inherit => line(out, i, "secrets: inherit"),
                        CallSecretsPlan::Map(m) => render_pairs(out, i, "secrets", m),
                    }
                    match &c.workflow {
                        CalledWorkflow::Unresolved(r) => line(out, i, &format!("unresolved: {r}")),
                        CalledWorkflow::Planned {
                            name,
                            concurrency,
                            jobs,
                        } => {
                            line(out, i, &format!("workflow: {}", q(name)));
                            if let Some(cc) = concurrency {
                                line(out, i, &format!("concurrency: {}", render_conc(cc)));
                            }
                            render_jobs(out, i, jobs);
                        }
                    }
                }
            }
        }
    }
}

impl Plan {
    /// A stable, human-readable rendering (used by golden tests and for debugging).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            match e {
                PlanEntry::Failed(err) => {
                    line(&mut out, 0, &format!("failed {}", err.path));
                    for d in &err.diagnostics {
                        line(&mut out, 1, &format!("{}: {}", d.pos, d.message));
                    }
                }
                PlanEntry::Run(r) => {
                    line(&mut out, 0, &format!("run {} {}", r.workflow, q(&r.name)));
                    line(&mut out, 1, &format!("run-name: {}", q(&r.run_name)));
                    line(
                        &mut out,
                        1,
                        &format!("event: {} ref={} sha={}", r.event, r.git_ref, r.sha),
                    );
                    if let Some(s) = &r.schedule {
                        line(&mut out, 1, &format!("schedule: {}", q(s)));
                    }
                    if !r.inputs.is_empty() {
                        line(
                            &mut out,
                            1,
                            &format!(
                                "inputs: {}",
                                Value::from(r.inputs.clone()).to_json_compact()
                            ),
                        );
                    }
                    if let Some(c) = &r.concurrency {
                        line(&mut out, 1, &format!("concurrency: {}", render_conc(c)));
                    }
                    line(
                        &mut out,
                        1,
                        &format!("permissions: {}", render_perms(&r.permissions)),
                    );
                    render_jobs(&mut out, 1, &r.jobs);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{BeeEvent, Cause, ChangedPaths, PushEvent, map_event};
    use crate::graph::JobResult;

    fn repo() -> RepoInfo {
        RepoInfo {
            owner: "octo".into(),
            name: "app".into(),
            default_branch: "main".into(),
            default_branch_sha: "d".repeat(40),
            server_url: "https://beehive.test".into(),
            api_url: "https://beehive.test/api/v3".into(),
            graphql_url: "https://beehive.test/api/graphql".into(),
        }
    }

    fn push_event(r: &RepoInfo) -> ActionsEvent {
        let p = PushEvent {
            git_ref: "refs/heads/main".into(),
            before: "a".repeat(40),
            after: "b".repeat(40),
            created: false,
            deleted: false,
            forced: false,
            commit_count: 1,
            head_commit_message: Some("Fix the build\n\nDetails".into()),
            changed: ChangedPaths::Known(vec!["src/main.rs".into()]),
        };
        map_event(
            &BeeEvent::Push(p),
            &Cause {
                actor: "mona".into(),
                by_workflow_token: false,
            },
            r,
        )
        .remove(0)
    }

    fn plan_src(files: &[(&str, &str)]) -> Plan {
        let r = repo();
        let ev = push_event(&r);
        let files: BTreeMap<String, String> = files
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let input = PlanInput {
            repo: &r,
            files: &files,
            event: &ev,
            run: RunInfo {
                run_id: 1,
                run_number: 1,
                run_attempt: 1,
            },
            vars: Object::new(),
            resolver: None,
        };
        plan_event(&input)
    }

    #[test]
    fn partial_evaluation_and_secrets_stay_for_runtime() {
        let p = plan_src(&[(
            ".github/workflows/ci.yml",
            "on: push\nenv:\n  TOKEN: ${{ secrets.T }}\n  REF: ${{ github.ref_name }}\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github.sha }} ${{ steps.x.outputs.y }} ${{ github.workspace }}\n",
        )]);
        let text = p.render();
        assert!(text.contains("TOKEN: \"${{ secrets.T }}\""), "{text}");
        assert!(text.contains("REF: \"main\""), "{text}");
        assert!(
            text.contains(&format!(
                "echo {} ${{{{ steps.x.outputs.y }}}} ${{{{ github.workspace }}}}",
                "b".repeat(40)
            )),
            "{text}"
        );
    }

    #[test]
    fn deferred_jobs_and_replanning_with_needs() {
        let src = "on: push\njobs:\n  setup:\n    runs-on: ubuntu-latest\n    outputs:\n      matrix: ${{ steps.m.outputs.matrix }}\n    steps:\n      - id: m\n        run: echo\n  build:\n    needs: setup\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix: ${{ fromJSON(needs.setup.outputs.matrix) }}\n    steps:\n      - run: make\n";
        let p = plan_src(&[(".github/workflows/ci.yml", src)]);
        let PlanEntry::Run(run) = &p.entries[0] else {
            panic!("{p:?}")
        };
        assert!(matches!(run.jobs[1].body, JobBody::Deferred(_)));
        let r = repo();
        let ev = push_event(&r);
        let files = BTreeMap::new();
        let input = PlanInput {
            repo: &r,
            files: &files,
            event: &ev,
            run: RunInfo {
                run_id: 1,
                run_number: 1,
                run_attempt: 1,
            },
            vars: Object::new(),
            resolver: None,
        };
        let wf = parse_workflow(".github/workflows/ci.yml", src).unwrap();
        let mut outputs = Object::new();
        outputs.insert(
            "matrix",
            Value::from(r#"{"os":["ubuntu-latest","macos-latest"]}"#),
        );
        let mut needs = BTreeMap::new();
        needs.insert(
            "setup".to_owned(),
            JobOutcome {
                result: JobResult::Success,
                outputs,
            },
        );
        let job = plan_job(&wf, "build", None, &input, &needs).unwrap();
        let JobBody::Instances(legs) = job.body else {
            panic!()
        };
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[1].runs_on.labels, vec!["macos-latest"]);
        assert_eq!(legs[1].name, "build (macos-latest)");
    }

    #[test]
    fn matrix_too_large_is_too_large() {
        let src = "on: push\njobs:\n  a:\n    runs-on: x\n    strategy:\n      matrix:\n        a: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17]\n        b: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]\n    steps: [{run: x}]\n";
        let p = plan_src(&[(".github/workflows/ci.yml", src)]);
        let PlanEntry::Failed(e) = &p.entries[0] else {
            panic!()
        };
        assert_eq!(e.to_bee_error().reason, bee_core::Reason::TooLarge);
        assert_eq!(e.diagnostics[0].pos.line, 7);
    }

    #[test]
    fn custom_and_invalid_shells() {
        let p = plan_src(&[(
            ".github/workflows/ci.yml",
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: x\n        shell: perl {0}\n      - run: y\n        shell: python\n",
        )]);
        let t = p.render();
        assert!(t.contains("shell: perl {0} -> \"perl {0}\""), "{t}");
        assert!(t.contains("shell: python -> \"python {0}\""), "{t}");
        let p = plan_src(&[(
            ".github/workflows/ci.yml",
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: x\n        shell: zsh\n",
        )]);
        assert!(matches!(p.entries[0], PlanEntry::Failed(_)));
    }

    #[test]
    fn reusable_workflow_input_and_secret_validation() {
        let callee = "on:\n  workflow_call:\n    inputs:\n      flag:\n        type: boolean\n        required: true\n    secrets:\n      tok:\n        required: true\njobs:\n  x:\n    runs-on: x\n    steps: [{run: x}]\n";
        let fail = |caller: &str, needle: &str| {
            let p = plan_src(&[
                (".github/workflows/a.yml", caller),
                (".github/workflows/r.yml", callee),
            ]);
            let PlanEntry::Failed(e) = &p.entries[0] else {
                panic!("{}", p.render())
            };
            assert!(e.diagnostics[0].message.contains(needle), "{e:?}");
        };
        fail(
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/r.yml\n    secrets: inherit\n",
            "Input flag is required",
        );
        fail(
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/r.yml\n    with: {flag: 'yes'}\n    secrets: inherit\n",
            "must be a boolean",
        );
        fail(
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/r.yml\n    with: {flag: true, nope: 1}\n    secrets: inherit\n",
            "Invalid input, nope",
        );
        fail(
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/r.yml\n    with: {flag: true}\n",
            "Secret tok is required",
        );
        fail(
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/r.yml\n    with: {flag: true}\n    secrets: {tok: x, other: y}\n",
            "Invalid secret, other",
        );
        fail(
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/missing.yml\n",
            "was not found",
        );
        let p = plan_src(&[(
            ".github/workflows/a.yml",
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/a.yml\n",
        )]);
        let PlanEntry::Failed(e) = &p.entries[0] else {
            panic!()
        };
        assert!(
            e.diagnostics[0]
                .message
                .contains("missing a 'on.workflow_call'"),
            "{e:?}"
        );
    }

    #[test]
    fn nesting_limit() {
        let mut files: Vec<(String, String)> = Vec::new();
        files.push((
            ".github/workflows/top.yml".into(),
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/l1.yml\n".into(),
        ));
        for i in 1..=10 {
            files.push((
                format!(".github/workflows/l{i}.yml"),
                format!(
                    "on: workflow_call\njobs:\n  c:\n    uses: ./.github/workflows/l{}.yml\n",
                    i + 1
                ),
            ));
        }
        files.push((
            ".github/workflows/l11.yml".into(),
            "on: workflow_call\njobs:\n  x:\n    runs-on: x\n    steps: [{run: x}]\n".into(),
        ));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let p = plan_src(&refs);
        let top = p.entries.iter().find_map(|e| match e {
            PlanEntry::Failed(e)
                if e.path == ".github/workflows/top.yml"
                    || e.diagnostics[0].message.contains("nested") =>
            {
                Some(e.clone())
            }
            _ => None,
        });
        assert!(
            top.unwrap().diagnostics[0]
                .message
                .contains("nested at most 10")
        );
    }

    #[test]
    fn root_job_conditions_are_decided() {
        let p = plan_src(&[(
            ".github/workflows/ci.yml",
            "on: push\njobs:\n  a:\n    if: github.ref == 'refs/heads/dev'\n    runs-on: x\n    steps: [{run: x}]\n  b:\n    if: ${{ always() }}\n    runs-on: x\n    steps: [{run: x}]\n",
        )]);
        let PlanEntry::Run(run) = &p.entries[0] else {
            panic!()
        };
        assert_eq!(run.jobs[0].body, JobBody::Skipped);
        assert_eq!(run.jobs[1].decided, Some(true));
    }
}
