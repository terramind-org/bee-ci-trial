//! The parsed, validated form of a workflow file.

use crate::cron::Cron;
use crate::expr::{Condition, ContextProvider, ExprError, Template};
use crate::glob::PatternList;
use crate::value::{Object, Value};
use crate::yaml::Pos;

/// A value from the workflow file that may contain `${{ }}` expressions at any depth.
#[derive(Clone, Debug, PartialEq)]
pub enum Dyn {
    /// `null` or an empty value.
    Null(Pos),
    /// A boolean.
    Bool(bool, Pos),
    /// A number.
    Number(f64, Pos),
    /// A string, possibly with expressions.
    Str(Template, Pos),
    /// A sequence.
    Seq(Vec<Dyn>, Pos),
    /// A mapping, in source order.
    Map(Vec<(String, Dyn)>, Pos),
}

impl Dyn {
    /// Where the value starts.
    pub fn pos(&self) -> Pos {
        match self {
            Dyn::Null(p)
            | Dyn::Bool(_, p)
            | Dyn::Number(_, p)
            | Dyn::Str(_, p)
            | Dyn::Seq(_, p)
            | Dyn::Map(_, p) => *p,
        }
    }

    /// Whether any string in the value holds an expression.
    pub fn has_expressions(&self) -> bool {
        let mut any = false;
        self.for_each_template(&mut |t| any |= !t.is_literal());
        any
    }

    /// Calls `f` for every string template in the value.
    pub fn for_each_template(&self, f: &mut dyn FnMut(&Template)) {
        match self {
            Dyn::Str(t, _) => f(t),
            Dyn::Seq(items, _) => {
                for i in items {
                    i.for_each_template(f);
                }
            }
            Dyn::Map(entries, _) => {
                for (_, v) in entries {
                    v.for_each_template(f);
                }
            }
            _ => {}
        }
    }

    /// Evaluates every expression; a lone expression keeps its value's type.
    pub fn evaluate(&self, ctx: &dyn ContextProvider) -> Result<Value, ExprError> {
        Ok(match self {
            Dyn::Null(_) => Value::Null,
            Dyn::Bool(b, _) => Value::Bool(*b),
            Dyn::Number(n, _) => Value::Number(*n),
            Dyn::Str(t, _) => t.evaluate(ctx)?,
            Dyn::Seq(items, _) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(i.evaluate(ctx)?);
                }
                Value::array(out)
            }
            Dyn::Map(entries, _) => {
                let mut o = Object::new();
                for (k, v) in entries {
                    o.insert(k.clone(), v.evaluate(ctx)?);
                }
                Value::from(o)
            }
        })
    }

    /// The literal string, if this is a string with no expressions.
    pub fn as_literal_str(&self) -> Option<String> {
        match self {
            Dyn::Str(t, _) if t.is_literal() => Some(t.render()),
            _ => None,
        }
    }
}

/// An ordered list of `NAME: value` environment entries.
pub type EnvMap = Vec<(String, Dyn)>;

/// A filter on refs or paths: include patterns, or ignore patterns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// `branches` / `tags` / `paths`: run when a pattern includes the name.
    Include(PatternList),
    /// `branches-ignore` / `tags-ignore` / `paths-ignore`: skip when a pattern includes it.
    Ignore(PatternList),
}

impl Filter {
    /// Whether one name passes the filter.
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Filter::Include(p) => p.matches(name),
            Filter::Ignore(p) => !p.matches(name),
        }
    }
}

/// The type of a `workflow_dispatch` or `workflow_call` input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputType {
    /// `string` (the default for dispatch inputs).
    String,
    /// `boolean`.
    Boolean,
    /// `number`.
    Number,
    /// `choice` (dispatch only), with `options`.
    Choice,
    /// `environment` (dispatch only).
    Environment,
}

impl InputType {
    /// The keyword.
    pub fn name(self) -> &'static str {
        match self {
            InputType::String => "string",
            InputType::Boolean => "boolean",
            InputType::Number => "number",
            InputType::Choice => "choice",
            InputType::Environment => "environment",
        }
    }
}

/// A declared input.
#[derive(Clone, Debug, PartialEq)]
pub struct InputDecl {
    /// Input id.
    pub id: String,
    /// `description`.
    pub description: Option<String>,
    /// `required` (default false).
    pub required: bool,
    /// `type`.
    pub kind: InputType,
    /// `default`; may hold expressions for `workflow_call` inputs.
    pub default: Option<Dyn>,
    /// `options` for `choice` inputs.
    pub options: Vec<String>,
    /// Where the input is declared.
    pub pos: Pos,
}

/// A `workflow_call` output.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputDecl {
    /// Output id.
    pub id: String,
    /// `description`.
    pub description: Option<String>,
    /// `value`, usually `${{ jobs.<id>.outputs.<name> }}`.
    pub value: Template,
}

/// A `workflow_call` secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretDecl {
    /// Secret id.
    pub id: String,
    /// `description`.
    pub description: Option<String>,
    /// `required`.
    pub required: bool,
}

/// One event under `on`, with its filters.
#[derive(Clone, Debug, PartialEq)]
pub struct EventTrigger {
    /// The event name (`push`, `pull_request`, ...).
    pub name: String,
    /// `types`, when given.
    pub types: Option<Vec<String>>,
    /// `branches` or `branches-ignore`.
    pub branches: Option<Filter>,
    /// `tags` or `tags-ignore` (`push` only).
    pub tags: Option<Filter>,
    /// `paths` or `paths-ignore`.
    pub paths: Option<Filter>,
    /// `workflows` (`workflow_run` only), matched against workflow names.
    pub workflows: Option<PatternList>,
    /// `cron` entries (`schedule` only).
    pub schedules: Vec<Cron>,
    /// `inputs` (`workflow_dispatch`, `workflow_call`).
    pub inputs: Vec<InputDecl>,
    /// `outputs` (`workflow_call`).
    pub outputs: Vec<OutputDecl>,
    /// `secrets` (`workflow_call`).
    pub secrets: Vec<SecretDecl>,
    /// Where the event is named.
    pub pos: Pos,
}

impl EventTrigger {
    /// A trigger with no filters.
    pub fn bare(name: &str, pos: Pos) -> EventTrigger {
        EventTrigger {
            name: name.to_owned(),
            types: None,
            branches: None,
            tags: None,
            paths: None,
            workflows: None,
            schedules: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            secrets: Vec::new(),
            pos,
        }
    }
}

/// A `GITHUB_TOKEN` permission level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// `none`.
    None,
    /// `read`.
    Read,
    /// `write`.
    Write,
}

impl Access {
    /// The keyword.
    pub fn name(self) -> &'static str {
        match self {
            Access::None => "none",
            Access::Read => "read",
            Access::Write => "write",
        }
    }
}

/// The permission scopes GitHub documents for `permissions`.
pub const PERMISSION_SCOPES: [&str; 16] = [
    "actions",
    "artifact-metadata",
    "attestations",
    "checks",
    "contents",
    "deployments",
    "discussions",
    "id-token",
    "issues",
    "models",
    "packages",
    "pages",
    "pull-requests",
    "repository-projects",
    "security-events",
    "statuses",
];

/// A `permissions` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Permissions {
    /// `read-all`.
    ReadAll,
    /// `write-all`.
    WriteAll,
    /// Individual scopes; unlisted scopes are `none`. `{}` is an empty list.
    Scopes(Vec<(String, Access)>),
}

/// A `concurrency` block.
#[derive(Clone, Debug, PartialEq)]
pub struct Concurrency {
    /// `group`.
    pub group: Template,
    /// `cancel-in-progress`: a boolean or an expression.
    pub cancel_in_progress: Dyn,
    /// `queue: max` (up to 100 pending) instead of the default `single`.
    pub queue_max: bool,
    /// Where the block is.
    pub pos: Pos,
}

/// `defaults.run`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunDefaults {
    /// `shell`.
    pub shell: Option<Dyn>,
    /// `working-directory`.
    pub working_directory: Option<Dyn>,
}

/// A reference to an action in `steps[*].uses`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionRef {
    /// `owner/repo[/path]@ref`.
    Repo {
        /// Owner.
        owner: String,
        /// Repository.
        repo: String,
        /// Subdirectory within the repository, if any.
        path: Option<String>,
        /// Branch, tag or SHA.
        git_ref: String,
    },
    /// `./path` in the workflow's repository.
    Local(String),
    /// `docker://image`.
    Docker(String),
}

impl ActionRef {
    /// Renders the reference as written.
    pub fn render(&self) -> String {
        match self {
            ActionRef::Repo {
                owner,
                repo,
                path,
                git_ref,
            } => match path {
                Some(p) => format!("{owner}/{repo}/{p}@{git_ref}"),
                None => format!("{owner}/{repo}@{git_ref}"),
            },
            ActionRef::Local(p) => p.clone(),
            ActionRef::Docker(i) => format!("docker://{i}"),
        }
    }
}

/// A reference to a reusable workflow in `jobs.<id>.uses`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkflowRef {
    /// `./.github/workflows/x.yml` in the same repository and commit.
    Local(String),
    /// `owner/repo/.github/workflows/x.yml@ref`.
    Remote {
        /// Owner.
        owner: String,
        /// Repository.
        repo: String,
        /// Path of the workflow file.
        path: String,
        /// Branch, tag or SHA.
        git_ref: String,
    },
}

impl WorkflowRef {
    /// Renders the reference as written.
    pub fn render(&self) -> String {
        match self {
            WorkflowRef::Local(p) => format!("./{p}"),
            WorkflowRef::Remote {
                owner,
                repo,
                path,
                git_ref,
            } => format!("{owner}/{repo}/{path}@{git_ref}"),
        }
    }
}

/// What a step does.
#[derive(Clone, Debug, PartialEq)]
pub enum StepAction {
    /// `run`.
    Run {
        /// The script.
        script: Template,
        /// `shell`.
        shell: Option<Dyn>,
        /// `working-directory`.
        working_directory: Option<Dyn>,
    },
    /// `uses`.
    Uses {
        /// The action.
        action: ActionRef,
        /// `with` inputs.
        with: Vec<(String, Dyn)>,
    },
}

/// A step.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// `id`.
    pub id: Option<String>,
    /// `name`.
    pub name: Option<Template>,
    /// `if`.
    pub condition: Option<Condition>,
    /// `run` or `uses`.
    pub action: StepAction,
    /// `env`.
    pub env: EnvMap,
    /// `continue-on-error`.
    pub continue_on_error: Option<Dyn>,
    /// `timeout-minutes`.
    pub timeout_minutes: Option<Dyn>,
    /// Where the step starts.
    pub pos: Pos,
}

/// `strategy`.
#[derive(Clone, Debug, PartialEq)]
pub struct Strategy {
    /// `matrix`: a mapping or an expression.
    pub matrix: Option<Dyn>,
    /// `fail-fast` (default true).
    pub fail_fast: Option<Dyn>,
    /// `max-parallel`.
    pub max_parallel: Option<Dyn>,
    /// Where the block is.
    pub pos: Pos,
}

/// A container for `container` or a `services` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Container {
    /// `image` (or the string form).
    pub image: Dyn,
    /// `credentials`.
    pub credentials: Option<Dyn>,
    /// `env`.
    pub env: EnvMap,
    /// `ports`.
    pub ports: Vec<Dyn>,
    /// `volumes`.
    pub volumes: Vec<Dyn>,
    /// `options`.
    pub options: Option<Dyn>,
    /// Where it is declared.
    pub pos: Pos,
}

/// A job that runs steps on a runner.
#[derive(Clone, Debug, PartialEq)]
pub struct StepsJob {
    /// `runs-on`.
    pub runs_on: Dyn,
    /// `environment`.
    pub environment: Option<Dyn>,
    /// `outputs`.
    pub outputs: Vec<(String, Template)>,
    /// `env`.
    pub env: EnvMap,
    /// `defaults.run`.
    pub defaults: RunDefaults,
    /// `timeout-minutes`.
    pub timeout_minutes: Option<Dyn>,
    /// `continue-on-error`.
    pub continue_on_error: Option<Dyn>,
    /// `container`.
    pub container: Option<Container>,
    /// `services`.
    pub services: Vec<(String, Container)>,
    /// `steps`.
    pub steps: Vec<Step>,
}

/// `secrets` of a reusable workflow call.
#[derive(Clone, Debug, PartialEq)]
pub enum CallSecrets {
    /// No secrets passed.
    None,
    /// `secrets: inherit`.
    Inherit,
    /// Explicit `name: value` pairs.
    Map(Vec<(String, Dyn)>),
}

/// A job that calls a reusable workflow.
#[derive(Clone, Debug, PartialEq)]
pub struct CallJob {
    /// `uses`.
    pub uses: WorkflowRef,
    /// `with`.
    pub with: Vec<(String, Dyn)>,
    /// `secrets`.
    pub secrets: CallSecrets,
}

/// The two kinds of job.
#[derive(Clone, Debug, PartialEq)]
pub enum JobKind {
    /// Runs steps.
    Steps(Box<StepsJob>),
    /// Calls a reusable workflow.
    Call(Box<CallJob>),
}

/// A job.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// The job id (the key under `jobs`).
    pub id: String,
    /// `name`.
    pub name: Option<Template>,
    /// `needs`, in the order written.
    pub needs: Vec<String>,
    /// `if`.
    pub condition: Option<Condition>,
    /// `permissions`.
    pub permissions: Option<Permissions>,
    /// `strategy`.
    pub strategy: Option<Strategy>,
    /// `concurrency`.
    pub concurrency: Option<Concurrency>,
    /// Steps or a call.
    pub kind: JobKind,
    /// Where the job is declared.
    pub pos: Pos,
}

/// A parsed workflow file.
#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    /// Path of the file in the repository.
    pub path: String,
    /// `name` (the display name defaults to the path).
    pub name: Option<String>,
    /// `run-name`.
    pub run_name: Option<Template>,
    /// Events under `on`, in the order written.
    pub on: Vec<EventTrigger>,
    /// `permissions`.
    pub permissions: Option<Permissions>,
    /// `env`.
    pub env: EnvMap,
    /// `defaults.run`.
    pub defaults: RunDefaults,
    /// `concurrency`.
    pub concurrency: Option<Concurrency>,
    /// `jobs`, in the order written.
    pub jobs: Vec<Job>,
}

impl Workflow {
    /// The display name: `name`, or the file path.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.path)
    }

    /// The trigger for an event, if the workflow listens to it.
    pub fn trigger(&self, event: &str) -> Option<&EventTrigger> {
        self.on.iter().find(|t| t.name == event)
    }

    /// Looks a job up by id (case-insensitively, as `needs` does).
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id.eq_ignore_ascii_case(id))
    }
}
