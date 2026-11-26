//! Executing one job plan (`plan_json`) in a temp workspace.
//!
//! Steps run in order. Each step's `if` is evaluated with the status functions bound to the
//! job so far (`success()` until a step fails, `cancelled()` once cancellation was
//! requested); `run` steps execute their script through the planned shell command with the
//! GitHub environment (`GITHUB_*`, `RUNNER_*`, `CI`), `GITHUB_OUTPUT`/`GITHUB_ENV`/
//! `GITHUB_PATH`/`GITHUB_STEP_SUMMARY`/`GITHUB_STATE` files and workflow commands on stdout;
//! `uses` steps run a built-in, composite or JavaScript action ([`super::actions`]), fetching
//! actions of other repositories through [`RunnerConfig::actions`]. `pre` scripts of the job's
//! JavaScript actions run before the first step and `post` scripts after the last, in reverse
//! order. Every line is masked before it is logged. A heartbeat renews the lease; a lost lease
//! stops the job without completing it.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bee_proto::v1::{
    CompleteRunnerJobRequest, KeyValue, RunnerJobAssignment, UpdateRunnerJobStepRequest,
    WorkflowConclusion, WorkflowStatus,
};
use bee_workflow::Value;
use bee_workflow::expr::{Condition, ContextProvider, FileHasher, Status, Template};
use bee_workflow::value::Object;
use serde_json::Value as Json;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::api::RunnerApi;
use crate::ci::convert::template_from_json;
use crate::ci::util::{mask, parse_command};
use crate::error::{ForgeError, Result};
use crate::ids::ts;

/// Local runner settings.
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    /// Directory jobs run in (one subdirectory per job, removed afterwards).
    pub work_root: PathBuf,
    /// Runner name (`runner.name`).
    pub name: String,
    /// Flush logs once this many bytes are buffered.
    pub chunk_bytes: usize,
    /// Lease renewal interval.
    pub heartbeat: Duration,
    /// Keep job directories after the job (debugging).
    pub keep_workspace: bool,
    /// Maximum wall-clock time allowed for one job.
    pub max_job_timeout: Duration,
    /// Where `uses: owner/repo@ref` actions come from; `None` rejects them.
    pub actions: Option<super::registry::ActionRegistry>,
}

impl Default for RunnerConfig {
    fn default() -> RunnerConfig {
        RunnerConfig {
            work_root: std::env::temp_dir().join("bee-runner"),
            name: "local".into(),
            chunk_bytes: 64 * 1024,
            heartbeat: Duration::from_secs(20),
            keep_workspace: false,
            max_job_timeout: Duration::from_secs(60 * 60),
            actions: None,
        }
    }
}

/// How a step ended.
#[derive(Clone, Debug, Default)]
pub struct StepOutcome {
    /// The step succeeded.
    pub ok: bool,
    /// Its outputs.
    pub outputs: BTreeMap<String, String>,
}

impl StepOutcome {
    /// A success with outputs.
    pub fn success(outputs: BTreeMap<String, String>) -> StepOutcome {
        StepOutcome { ok: true, outputs }
    }

    /// A failure.
    pub fn failure() -> StepOutcome {
        StepOutcome::default()
    }
}

/// What a step does, from a plan or a composite action.
#[derive(Clone, Debug)]
pub enum StepKind {
    /// A script through a shell command template (`{0}` = the script file).
    Run {
        /// Script.
        script: Template,
        /// Shell keyword.
        shell: String,
        /// Command template.
        command: String,
        /// Working directory.
        working_directory: Option<Template>,
    },
    /// An action.
    Uses {
        /// Reference as written.
        action: String,
        /// `with`.
        with: Vec<(String, Template)>,
    },
}

/// One step to execute.
#[derive(Clone, Debug)]
pub struct StepSpec {
    /// `id`.
    pub id: Option<String>,
    /// Display name.
    pub name: Template,
    /// `if` source.
    pub condition: String,
    /// What it does.
    pub kind: StepKind,
    /// Step `env`.
    pub env: Vec<(String, Template)>,
    /// `continue-on-error`.
    pub continue_on_error: Template,
    /// `timeout-minutes`.
    pub timeout_minutes: Option<Template>,
}

fn t(v: &Json) -> Template {
    template_from_json(v).unwrap_or_default()
}

fn pairs(v: &Json) -> Vec<(String, Template)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), t(p.get(1)?))))
                .collect()
        })
        .unwrap_or_default()
}

/// Steps of a plan.
pub fn plan_steps(plan: &Json) -> Vec<StepSpec> {
    plan["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|s| StepSpec {
                    id: s["id"].as_str().map(str::to_string),
                    name: t(&s["name"]),
                    condition: s["if"].as_str().unwrap_or("").to_string(),
                    kind: if s["kind"] == "run" {
                        StepKind::Run {
                            script: t(&s["script"]),
                            shell: s["shell"]["name"].as_str().unwrap_or("bash").to_string(),
                            command: s["shell"]["command"]
                                .as_str()
                                .unwrap_or("bash -e {0}")
                                .to_string(),
                            working_directory: (!s["working_directory"].is_null())
                                .then(|| t(&s["working_directory"])),
                        }
                    } else {
                        StepKind::Uses {
                            action: s["action"].as_str().unwrap_or("").to_string(),
                            with: pairs(&s["with"]),
                        }
                    },
                    env: pairs(&s["env"]),
                    continue_on_error: t(&s["continue_on_error"]),
                    timeout_minutes: (!s["timeout_minutes"].is_null())
                        .then(|| t(&s["timeout_minutes"])),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Buffers log lines and appends them to the server in chunks.
pub struct LogSink {
    api: Arc<dyn RunnerApi>,
    job_id: String,
    lease_id: String,
    offset: u64,
    buf: Vec<u8>,
    step: u32,
    chunk: usize,
}

impl LogSink {
    fn new(api: Arc<dyn RunnerApi>, a: &RunnerJobAssignment, chunk: usize) -> LogSink {
        LogSink {
            api,
            job_id: a.job_id.clone(),
            lease_id: a.lease_id.clone(),
            offset: 0,
            buf: Vec::new(),
            step: 0,
            chunk,
        }
    }

    /// Flushes buffered bytes.
    pub async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.buf);
        for part in data.chunks(crate::ci::logs::MAX_CHUNK) {
            self.offset = self
                .api
                .append_log(
                    &self.job_id,
                    &self.lease_id,
                    self.offset,
                    part.to_vec(),
                    self.step,
                )
                .await?;
        }
        Ok(())
    }

    async fn set_step(&mut self, step: u32) -> Result<()> {
        self.flush().await?;
        self.step = step;
        Ok(())
    }
}

/// One running job.
pub struct Job {
    pub(crate) api: Arc<dyn RunnerApi>,
    pub(crate) a: RunnerJobAssignment,
    pub(crate) plan: Json,
    pub(crate) log: LogSink,
    pub(crate) workspace: PathBuf,
    pub(crate) temp: PathBuf,
    pub(crate) root: PathBuf,
    _workspace_guard: WorkspaceGuard,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) path_add: Vec<String>,
    pub(crate) masks: Vec<String>,
    pub(crate) secrets: BTreeMap<String, String>,
    pub(crate) failed: bool,
    pub(crate) timed_out: bool,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) force: Arc<AtomicBool>,
    pub(crate) lost: Arc<AtomicBool>,
    pub(crate) summary: String,
    pub(crate) post: Vec<super::actions::PostAction>,
    pub(crate) config: RunnerConfig,
    file_seq: u32,
    /// The action whose steps or script run now (`github.action_path`, `GITHUB_ACTION_*`).
    pub(crate) action: Option<ActionInfo>,
    /// `GITHUB_STATE` values written by the last process.
    pub(crate) last_state: BTreeMap<String, String>,
    /// State saved by `pre` scripts, by top-level step key.
    pub(crate) pre_states: BTreeMap<String, BTreeMap<String, String>>,
    /// Key of the top-level step running now (`step<n>`).
    pub(crate) step_key: String,
    /// Fetched remote actions: reference → directory.
    pub(crate) fetched: BTreeMap<String, PathBuf>,
    job_deadline: tokio::time::Instant,
}

/// Removes a job's private directory if its task is cancelled before normal cleanup.
struct WorkspaceGuard {
    root: PathBuf,
    keep_workspace: bool,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        if !self.keep_workspace {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// The action a step is running.
#[derive(Clone, Debug, Default)]
pub struct ActionInfo {
    /// Its directory.
    pub path: PathBuf,
    /// `owner/repo` (empty for local actions).
    pub repository: String,
    /// The ref it was fetched at.
    pub git_ref: String,
}

struct Ctx {
    contexts: Object,
    status: Status,
    workspace: PathBuf,
}

impl ContextProvider for Ctx {
    fn context(&self, name: &str) -> Option<Value> {
        self.contexts.get(name).cloned()
    }
    fn status(&self) -> Status {
        self.status
    }
    fn file_hasher(&self) -> Option<&dyn FileHasher> {
        Some(self)
    }
}

impl FileHasher for Ctx {
    fn hash_files(&self, patterns: &[String]) -> std::result::Result<String, String> {
        use sha2::{Digest, Sha256};
        let list = bee_workflow::glob::PatternList::new(patterns);
        let mut files = Vec::new();
        walk(&self.workspace, &mut files);
        files.sort();
        let root = std::fs::canonicalize(&self.workspace).map_err(|e| e.to_string())?;
        let mut outer = Sha256::new();
        let mut any = false;
        for f in files {
            let rel = f
                .strip_prefix(&self.workspace)
                .unwrap_or(&f)
                .to_string_lossy()
                .replace('\\', "/");
            if list.matches(&rel) {
                let resolved = std::fs::canonicalize(&f).map_err(|e| e.to_string())?;
                if !resolved.starts_with(&root) {
                    return Err(format!("{rel} resolves outside the workspace"));
                }
                let bytes = std::fs::read(resolved).map_err(|e| e.to_string())?;
                outer.update(Sha256::digest(&bytes));
                any = true;
            }
        }
        Ok(if any {
            crate::ids::hex(&outer.finalize())
        } else {
            String::new()
        })
    }
}

/// Every file under `dir`.
pub(crate) fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            out.push(p);
        } else if metadata.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn obj(v: &Json) -> Value {
    crate::ci::util::from_json(v)
}

fn split_command(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in cmd.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parses a `GITHUB_OUTPUT`/`GITHUB_ENV` file (`k=v` and `k<<DELIM` blocks).
pub fn parse_kv_file(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some((k, delim)) = line.split_once("<<")
            && !k.contains('=')
        {
            let mut v = Vec::new();
            for l in lines.by_ref() {
                if l == delim {
                    break;
                }
                v.push(l);
            }
            out.push((k.to_string(), v.join("\n")));
        } else if let Some((k, v)) = line.split_once('=') {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

impl Job {
    pub(crate) async fn start(
        api: Arc<dyn RunnerApi>,
        a: RunnerJobAssignment,
        config: RunnerConfig,
    ) -> Result<Job> {
        let plan: Json = serde_json::from_str(&a.plan_json)
            .map_err(|e| ForgeError::invalid("bad_plan", format!("plan_json: {e}")))?;
        let root = config.work_root.join(&a.job_id);
        let workspace = root.join("workspace");
        let temp = root.join("temp");
        let home = root.join("home");
        for d in [&workspace, &temp, &home] {
            tokio::fs::create_dir_all(d)
                .await
                .map_err(|e| ForgeError::internal(format!("workspace: {e}")))?;
        }
        let secrets = api.secrets(&a).await?;
        let job_timeout = plan["timeout_minutes"]
            .as_f64()
            .filter(|m| m.is_finite() && *m > 0.0)
            .map(|m| (m * 60.0).min(config.max_job_timeout.as_secs_f64()))
            .unwrap_or(config.max_job_timeout.as_secs_f64());
        let mut masks: Vec<String> = secrets.values().cloned().collect();
        masks.push(a.job_token.clone());
        let log = LogSink::new(api.clone(), &a, config.chunk_bytes);
        Ok(Job {
            api,
            a,
            plan,
            log,
            workspace,
            temp,
            root: root.clone(),
            _workspace_guard: WorkspaceGuard {
                root: root.clone(),
                keep_workspace: config.keep_workspace,
            },
            env: BTreeMap::new(),
            path_add: Vec::new(),
            masks,
            secrets,
            failed: false,
            timed_out: false,
            cancel: Arc::new(AtomicBool::new(false)),
            force: Arc::new(AtomicBool::new(false)),
            lost: Arc::new(AtomicBool::new(false)),
            summary: String::new(),
            post: Vec::new(),
            config,
            file_seq: 0,
            action: None,
            last_state: BTreeMap::new(),
            pre_states: BTreeMap::new(),
            step_key: String::new(),
            fetched: BTreeMap::new(),
            job_deadline: tokio::time::Instant::now() + Duration::from_secs_f64(job_timeout),
        })
    }

    /// Writes one log line (masked).
    pub async fn line(&mut self, text: &str) -> Result<()> {
        let masked = mask(text, &self.masks);
        self.log.buf.extend_from_slice(masked.as_bytes());
        self.log.buf.push(b'\n');
        if self.log.buf.len() >= self.log.chunk {
            self.log.flush().await?;
        }
        Ok(())
    }

    fn github(&self) -> Value {
        let mut g = obj(&self.plan["github"])
            .as_object()
            .cloned()
            .unwrap_or_default();
        g.insert(
            "workspace",
            Value::from(self.workspace.to_string_lossy().as_ref()),
        );
        g.insert(
            "job",
            Value::from(self.plan["job_key"].as_str().unwrap_or("")),
        );
        g.insert("token", Value::from(self.a.job_token.as_str()));
        g.insert("action", Value::from(""));
        let a = self.action.clone().unwrap_or_default();
        g.insert(
            "action_path",
            Value::from(a.path.to_string_lossy().as_ref()),
        );
        g.insert("action_repository", Value::from(a.repository.as_str()));
        g.insert("action_ref", Value::from(a.git_ref.as_str()));
        Value::from(g)
    }

    fn runner(&self) -> Value {
        Value::object([
            ("name", Value::from(self.config.name.as_str())),
            ("os", Value::from(runner_os())),
            ("arch", Value::from(runner_arch())),
            ("temp", Value::from(self.temp.to_string_lossy().as_ref())),
            (
                "tool_cache",
                Value::from(self.root.join("tools").to_string_lossy().as_ref()),
            ),
            (
                "debug",
                Value::from(if self.plan["debug"].as_bool().unwrap_or(false) {
                    "1"
                } else {
                    ""
                }),
            ),
            ("environment", Value::from("self-hosted")),
        ])
    }

    pub(crate) fn status(&self) -> Status {
        let cancelled = self.cancel.load(Ordering::SeqCst);
        Status {
            success: !self.failed && !cancelled,
            failure: self.failed && !cancelled,
            cancelled,
        }
    }

    /// The expression contexts for a step (`inputs` and `steps` from `scope`).
    pub(crate) fn ctx(
        &self,
        scope: &Scope,
        extra_env: &BTreeMap<String, String>,
    ) -> impl ContextProvider + use<> {
        let mut c = Object::new();
        let mut env = Object::new();
        for (k, v) in self.env.iter().chain(extra_env.iter()) {
            env.insert(k.clone(), Value::from(v.as_str()));
        }
        let mut secrets: Object = self
            .secrets
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect();
        secrets.insert("GITHUB_TOKEN", Value::from(self.a.job_token.as_str()));
        c.insert("github", self.github());
        c.insert("env", Value::from(env));
        c.insert("vars", obj(&self.plan["vars"]));
        c.insert("secrets", Value::from(secrets));
        c.insert(
            "inputs",
            scope
                .inputs
                .clone()
                .unwrap_or_else(|| obj(&self.plan["inputs"])),
        );
        c.insert("matrix", obj(&self.plan["matrix"]));
        c.insert("strategy", obj(&self.plan["strategy"]));
        c.insert("needs", obj(&self.plan["needs"]));
        c.insert("runner", self.runner());
        c.insert(
            "job",
            Value::object([(
                "status",
                Value::from(if self.cancel.load(Ordering::SeqCst) {
                    "cancelled"
                } else if self.failed {
                    "failure"
                } else {
                    "success"
                }),
            )]),
        );
        c.insert("steps", Value::from(scope.steps.clone()));
        Ctx {
            contexts: c,
            status: self.status(),
            workspace: self.workspace.clone(),
        }
    }

    fn base_env(&self) -> BTreeMap<String, String> {
        let g = &self.plan["github"];
        let s = |k: &str| g[k].as_str().unwrap_or("").to_string();
        let mut e = BTreeMap::new();
        e.insert("CI".into(), "true".into());
        e.insert("GITHUB_ACTIONS".into(), "true".into());
        e.insert("BEE_ACTIONS".into(), "true".into());
        e.insert(
            "GITHUB_WORKSPACE".into(),
            self.workspace.to_string_lossy().into_owned(),
        );
        for (env, key) in [
            ("GITHUB_SHA", "sha"),
            ("GITHUB_REF", "ref"),
            ("GITHUB_REF_NAME", "ref_name"),
            ("GITHUB_REF_TYPE", "ref_type"),
            ("GITHUB_REPOSITORY", "repository"),
            ("GITHUB_REPOSITORY_OWNER", "repository_owner"),
            ("GITHUB_RUN_ID", "run_id"),
            ("GITHUB_RUN_NUMBER", "run_number"),
            ("GITHUB_RUN_ATTEMPT", "run_attempt"),
            ("GITHUB_ACTOR", "actor"),
            ("GITHUB_TRIGGERING_ACTOR", "triggering_actor"),
            ("GITHUB_EVENT_NAME", "event_name"),
            ("GITHUB_SERVER_URL", "server_url"),
            ("GITHUB_API_URL", "api_url"),
            ("GITHUB_GRAPHQL_URL", "graphql_url"),
            ("GITHUB_WORKFLOW", "workflow"),
            ("GITHUB_WORKFLOW_REF", "workflow_ref"),
            ("GITHUB_HEAD_REF", "head_ref"),
            ("GITHUB_BASE_REF", "base_ref"),
        ] {
            e.insert(env.into(), s(key));
        }
        e.insert(
            "GITHUB_JOB".into(),
            self.plan["job_key"].as_str().unwrap_or("").to_string(),
        );
        e.insert(
            "GITHUB_EVENT_PATH".into(),
            self.temp.join("event.json").to_string_lossy().into_owned(),
        );
        e.insert("RUNNER_OS".into(), runner_os().into());
        e.insert("RUNNER_ARCH".into(), runner_arch().into());
        e.insert("RUNNER_NAME".into(), self.config.name.clone());
        e.insert(
            "RUNNER_TEMP".into(),
            self.temp.to_string_lossy().into_owned(),
        );
        e.insert(
            "RUNNER_TOOL_CACHE".into(),
            self.root.join("tools").to_string_lossy().into_owned(),
        );
        if self.plan["debug"].as_bool().unwrap_or(false) {
            e.insert("RUNNER_DEBUG".into(), "1".into());
            e.insert("ACTIONS_STEP_DEBUG".into(), "true".into());
        }
        if let Some(a) = &self.action {
            e.insert(
                "GITHUB_ACTION_PATH".into(),
                a.path.to_string_lossy().into_owned(),
            );
            e.insert("GITHUB_ACTION_REPOSITORY".into(), a.repository.clone());
            e.insert("GITHUB_ACTION_REF".into(), a.git_ref.clone());
        }
        e
    }

    /// Runs a script step; returns its outcome.
    pub(crate) async fn run_script(
        &mut self,
        script: &str,
        shell: &str,
        command: &str,
        wd: Option<&str>,
        env: &BTreeMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<StepOutcome> {
        self.file_seq += 1;
        let n = self.file_seq;
        let ext = match shell {
            "python" => "py",
            "pwsh" | "powershell" => "ps1",
            "cmd" => "cmd",
            _ => "sh",
        };
        let script_path = self.temp.join(format!("step{n}.{ext}"));
        std::fs::write(&script_path, script)
            .map_err(|e| ForgeError::internal(format!("script: {e}")))?;
        let argv: Vec<String> = split_command(command)
            .into_iter()
            .map(|a| a.replace("{0}", &script_path.to_string_lossy()))
            .collect();
        if argv.is_empty() {
            self.line(&format!("##[error]empty shell command {command:?}"))
                .await?;
            return Ok(StepOutcome::failure());
        }
        let dir = match wd {
            Some(w) if !w.is_empty() => {
                let p = Path::new(w);
                let invalid = p.is_absolute()
                    || p.components()
                        .any(|component| matches!(component, Component::ParentDir));
                let candidate = self.workspace.join(p);
                let workspace = std::fs::canonicalize(&self.workspace);
                let resolved = std::fs::canonicalize(&candidate);
                match (invalid, workspace, resolved) {
                    (false, Ok(workspace), Ok(resolved)) if resolved.starts_with(&workspace) => {
                        resolved
                    }
                    _ => {
                        self.line(&format!(
                            "##[error]working-directory must stay inside the workspace: {w}"
                        ))
                        .await?;
                        return Ok(StepOutcome::failure());
                    }
                }
            }
            _ => self.workspace.clone(),
        };
        self.run_argv(&argv, &dir, env, timeout).await
    }

    /// Time remaining before the job's runner-enforced deadline.
    pub(crate) fn remaining_timeout(&self) -> Duration {
        self.job_deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }

    /// Runs one process with the GitHub environment and file commands (`GITHUB_OUTPUT`,
    /// `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_STEP_SUMMARY`, `GITHUB_STATE`); the state it saved
    /// is left in `last_state`.
    pub(crate) async fn run_argv(
        &mut self,
        argv: &[String],
        dir: &Path,
        env: &BTreeMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<StepOutcome> {
        self.file_seq += 1;
        let n = self.file_seq;
        let out_path = self.temp.join(format!("output{n}"));
        let env_path = self.temp.join(format!("env{n}"));
        let path_path = self.temp.join(format!("path{n}"));
        let summary_path = self.temp.join(format!("summary{n}"));
        let state_path = self.temp.join(format!("state{n}"));
        for p in [&out_path, &env_path, &path_path, &summary_path, &state_path] {
            std::fs::write(p, b"")
                .map_err(|e| ForgeError::internal(format!("runner files: {e}")))?;
        }
        self.last_state.clear();
        let Some((prog, args)) = argv.split_first() else {
            self.line("##[error]empty command").await?;
            return Ok(StepOutcome::failure());
        };
        let mut full_env = self.base_env();
        full_env.extend(self.env.clone());
        full_env.extend(env.clone());
        full_env.insert(
            "GITHUB_OUTPUT".into(),
            out_path.to_string_lossy().into_owned(),
        );
        full_env.insert("GITHUB_ENV".into(), env_path.to_string_lossy().into_owned());
        full_env.insert(
            "GITHUB_PATH".into(),
            path_path.to_string_lossy().into_owned(),
        );
        full_env.insert(
            "GITHUB_STEP_SUMMARY".into(),
            summary_path.to_string_lossy().into_owned(),
        );
        full_env.insert(
            "GITHUB_STATE".into(),
            state_path.to_string_lossy().into_owned(),
        );
        let mut path = std::env::var("PATH").unwrap_or_default();
        for p in self.path_add.iter().rev() {
            path = format!("{p}:{path}");
        }
        full_env.insert("PATH".into(), path);
        full_env.insert(
            "HOME".into(),
            self.root.join("home").to_string_lossy().into_owned(),
        );
        if let Ok(lang) = std::env::var("LANG") {
            full_env.insert("LANG".into(), lang);
        }
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args)
            .current_dir(dir)
            .env_clear()
            .envs(&full_env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.line(&format!("##[error]could not start `{prog}`: {e}"))
                    .await?;
                return Ok(StepOutcome::failure());
            }
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
        if let Some(out) = child.stdout.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(l).await.is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(err) = child.stderr.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(l).await.is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let deadline = timeout.map(|d| tokio::time::Instant::now() + d);
        let mut outputs = BTreeMap::new();
        let mut exit: Option<std::process::ExitStatus> = None;
        let mut killed = None;
        let cancel_at_start = self.cancel.load(Ordering::SeqCst);
        loop {
            tokio::select! {
                line = rx.recv() => match line {
                    Some(l) => self.handle_output(&l, &mut outputs).await?,
                    None => {
                        if exit.is_some() || killed.is_some() {
                            break;
                        }
                        match child.wait().await {
                            Ok(s) => exit = Some(s),
                            Err(e) => { self.line(&format!("##[error]{e}")).await?; break; }
                        }
                        break;
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    let cancelled = (self.cancel.load(Ordering::SeqCst) && !cancel_at_start) || self.force.load(Ordering::SeqCst) || self.lost.load(Ordering::SeqCst);
                    if cancelled && killed.is_none() {
                        kill_tree(&mut child).await;
                        killed = Some("The operation was canceled.");
                    } else if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) && killed.is_none() {
                        kill_tree(&mut child).await;
                        if tokio::time::Instant::now() >= self.job_deadline {
                            self.timed_out = true;
                        }
                        killed = Some("The action has timed out.");
                    }
                }
            }
        }
        if exit.is_none() && killed.is_none() {
            exit = child.wait().await.ok();
        }
        if let Some(msg) = killed {
            self.line(&format!("##[error]{msg}")).await?;
            return Ok(StepOutcome::failure());
        }
        for (k, v) in parse_kv_file(&std::fs::read_to_string(&out_path).unwrap_or_default()) {
            outputs.insert(k, v);
        }
        for (k, v) in parse_kv_file(&std::fs::read_to_string(&env_path).unwrap_or_default()) {
            self.env.insert(k, v);
        }
        for (k, v) in parse_kv_file(&std::fs::read_to_string(&state_path).unwrap_or_default()) {
            self.last_state.insert(k, v);
        }
        for p in std::fs::read_to_string(&path_path)
            .unwrap_or_default()
            .lines()
        {
            if !p.trim().is_empty() {
                self.path_add.push(p.trim().to_string());
            }
        }
        let summary = std::fs::read_to_string(&summary_path).unwrap_or_default();
        if !summary.is_empty() {
            self.summary.push_str(&mask(&summary, &self.masks));
        }
        let code = exit.and_then(|s| s.code());
        if code == Some(0) {
            Ok(StepOutcome::success(outputs))
        } else {
            self.line(&format!(
                "##[error]Process completed with exit code {}.",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            ))
            .await?;
            Ok(StepOutcome { ok: false, outputs })
        }
    }

    async fn handle_output(
        &mut self,
        line: &str,
        outputs: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        if let Some(c) = parse_command(line) {
            match c.name.as_str() {
                "add-mask" => {
                    if !c.message.is_empty() {
                        self.masks.push(c.message.clone());
                    }
                    return Ok(());
                }
                "set-output" => {
                    if let Some((_, n)) = c.props.iter().find(|(k, _)| k == "name") {
                        outputs.insert(n.clone(), c.message.clone());
                    }
                    return Ok(());
                }
                "save-state" => {
                    if let Some((_, n)) = c.props.iter().find(|(k, _)| k == "name") {
                        self.last_state.insert(n.clone(), c.message.clone());
                    }
                    return Ok(());
                }
                "debug" if !self.plan["debug"].as_bool().unwrap_or(false) => return Ok(()),
                _ => {}
            }
        }
        self.line(line).await
    }

    /// Runs a list of steps in `scope`; top-level steps report their status (`top`).
    pub(crate) async fn run_steps(
        &mut self,
        steps: &[StepSpec],
        scope: &mut Scope,
        top: bool,
    ) -> Result<()> {
        self.run_steps_from(steps, scope, top, 0).await
    }

    async fn run_steps_from(
        &mut self,
        steps: &[StepSpec],
        scope: &mut Scope,
        top: bool,
        start: usize,
    ) -> Result<()> {
        for (i, s) in steps.iter().enumerate() {
            if self.lost.load(Ordering::SeqCst) || self.force.load(Ordering::SeqCst) {
                return Ok(());
            }
            let number = if top {
                (start + i) as u32 + 1
            } else {
                self.log.step
            };
            if top {
                self.step_key = format!("step{number}");
            }
            let step_env_ctx = BTreeMap::new();
            let ctx = self.ctx(scope, &step_env_ctx);
            let name = s
                .name
                .evaluate_string(&ctx)
                .unwrap_or_else(|_| s.name.render());
            let run = match Condition::parse(&s.condition) {
                Ok(c) => match c.evaluate(&ctx) {
                    Ok(b) => b,
                    Err(e) => {
                        self.line(&format!(
                            "##[error]The step condition `{}` failed to evaluate: {}",
                            s.condition, e.message
                        ))
                        .await?;
                        self.failed = true;
                        false
                    }
                },
                Err(e) => {
                    self.line(&format!(
                        "##[error]bad condition `{}`: {}",
                        s.condition, e.message
                    ))
                    .await?;
                    self.failed = true;
                    false
                }
            };
            let now = || crate::ids::ts(now_ms());
            if !run {
                if top {
                    self.api
                        .update_step(UpdateRunnerJobStepRequest {
                            job_id: self.a.job_id.clone(),
                            lease_id: self.a.lease_id.clone(),
                            step_number: number,
                            step_name: name.clone(),
                            status: WorkflowStatus::Completed as i32,
                            conclusion: WorkflowConclusion::Skipped as i32,
                            completed_at: Some(now()),
                            ..Default::default()
                        })
                        .await?;
                }
                if let Some(id) = &s.id {
                    scope.steps.insert(
                        id.clone(),
                        Value::object([
                            ("outputs", Value::from(Object::new())),
                            ("outcome", Value::from("skipped")),
                            ("conclusion", Value::from("skipped")),
                        ]),
                    );
                }
                continue;
            }
            if top {
                self.log.set_step(number).await?;
                self.api
                    .update_step(UpdateRunnerJobStepRequest {
                        job_id: self.a.job_id.clone(),
                        lease_id: self.a.lease_id.clone(),
                        step_number: number,
                        step_name: name.clone(),
                        status: WorkflowStatus::InProgress as i32,
                        started_at: Some(now()),
                        ..Default::default()
                    })
                    .await?;
            }
            self.line(&format!("##[group]{name}")).await?;
            let mut env = BTreeMap::new();
            for (k, v) in &s.env {
                let ctx = self.ctx(scope, &env);
                env.insert(k.clone(), v.evaluate_string(&ctx).unwrap_or_default());
            }
            let ctx = self.ctx(scope, &env);
            let coe = match s.continue_on_error.evaluate(&ctx) {
                Ok(Value::Bool(b)) => b,
                Ok(Value::String(v)) => v.trim() == "true",
                Ok(v) => v.is_truthy(),
                Err(_) => false,
            };
            let step_timeout = s
                .timeout_minutes
                .as_ref()
                .and_then(|t| t.evaluate(&ctx).ok())
                .map(|v| v.to_number())
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|m| Duration::from_secs_f64(m * 60.0));
            let remaining = self
                .job_deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.timed_out = true;
                self.failed = true;
                self.line("##[error]The job has timed out.").await?;
                return Ok(());
            }
            let timeout = Some(step_timeout.map_or(remaining, |step| step.min(remaining)));
            let outcome = match &s.kind {
                StepKind::Run {
                    script,
                    shell,
                    command,
                    working_directory,
                } => {
                    let script = match script.evaluate_string(&ctx) {
                        Ok(x) => x,
                        Err(e) => {
                            self.line(&format!("##[error]{}", e.message)).await?;
                            String::new()
                        }
                    };
                    let wd = working_directory
                        .as_ref()
                        .and_then(|w| w.evaluate_string(&ctx).ok());
                    for l in script.lines() {
                        self.line(&format!("[command]{l}")).await?;
                    }
                    self.line("##[endgroup]").await?;
                    self.run_script(&script, shell, command, wd.as_deref(), &env, timeout)
                        .await?
                }
                StepKind::Uses { action, with } => {
                    let mut w = BTreeMap::new();
                    for (k, v) in with {
                        w.insert(k.clone(), v.evaluate_string(&ctx).unwrap_or_default());
                    }
                    self.line(&format!(
                        "with: {}",
                        w.keys().cloned().collect::<Vec<_>>().join(", ")
                    ))
                    .await?;
                    self.line("##[endgroup]").await?;
                    match Box::pin(super::actions::run_action(
                        self, action, w, &env, scope, timeout,
                    ))
                    .await
                    {
                        Ok(o) => o,
                        Err(e) => {
                            self.line(&format!("##[error]{}", e.message)).await?;
                            StepOutcome::failure()
                        }
                    }
                }
            };
            let outcome_s = if outcome.ok { "success" } else { "failure" };
            let conclusion_ok = outcome.ok || coe;
            if !conclusion_ok {
                self.failed = true;
            }
            if let Some(id) = &s.id {
                let outs: Object = outcome
                    .outputs
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                    .collect();
                scope.steps.insert(
                    id.clone(),
                    Value::object([
                        ("outputs", Value::from(outs)),
                        ("outcome", Value::from(outcome_s)),
                        (
                            "conclusion",
                            Value::from(if conclusion_ok { "success" } else { "failure" }),
                        ),
                    ]),
                );
            }
            if top {
                self.log.flush().await?;
                let conclusion = if self.timed_out {
                    WorkflowConclusion::TimedOut
                } else if self.cancel.load(Ordering::SeqCst) && !outcome.ok {
                    WorkflowConclusion::Cancelled
                } else if conclusion_ok {
                    WorkflowConclusion::Success
                } else {
                    WorkflowConclusion::Failure
                };
                self.api
                    .update_step(UpdateRunnerJobStepRequest {
                        job_id: self.a.job_id.clone(),
                        lease_id: self.a.lease_id.clone(),
                        step_number: number,
                        step_name: name,
                        status: WorkflowStatus::Completed as i32,
                        conclusion: conclusion as i32,
                        completed_at: Some(now()),
                        ..Default::default()
                    })
                    .await?;
            }
            if self.timed_out {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Runs the job to completion and reports it; returns the conclusion (`None` when the
    /// lease was lost and the job was not completed).
    pub(crate) async fn execute(mut self) -> Result<Option<WorkflowConclusion>> {
        let event = self.plan["github"]["event"].clone();
        let _ = std::fs::write(self.temp.join("event.json"), event.to_string());
        let mut scope = Scope::default();
        let mut env = BTreeMap::new();
        for (k, v) in pairs(&self.plan["env"]) {
            let ctx = self.ctx(&scope, &env);
            env.insert(k, v.evaluate_string(&ctx).unwrap_or_default());
        }
        self.env.extend(env);
        let heartbeat = {
            let api = self.api.clone();
            let job = self.a.job_id.clone();
            let lease = self.a.lease_id.clone();
            let (cancel, force, lost) =
                (self.cancel.clone(), self.force.clone(), self.lost.clone());
            let every = self.config.heartbeat;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    match api.renew(&job, &lease).await {
                        Ok(r) => {
                            if r.cancel_requested {
                                cancel.store(true, Ordering::SeqCst);
                            }
                            if r.force_cancel {
                                force.store(true, Ordering::SeqCst);
                            }
                        }
                        Err(_) => {
                            lost.store(true, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            })
        };
        if let Some(what) = self.container_request() {
            self.line(&format!(
                "##[error]this runner cannot run containers: {what}"
            ))
            .await?;
            self.failed = true;
        } else {
            let steps = plan_steps(&self.plan);
            self.line(&format!(
                "Job {} on runner {}",
                self.plan["name"].as_str().unwrap_or(""),
                self.config.name
            ))
            .await?;
            let first_is_checkout = matches!(
                steps.first().map(|step| &step.kind),
                Some(StepKind::Uses { action, .. })
                    if action.split_once('@').map_or(action.as_str(), |(name, _)| name)
                        .eq_ignore_ascii_case("actions/checkout")
            );
            let has_local_actions = steps.iter().any(|step| {
                matches!(&step.kind, StepKind::Uses { action, .. } if action.starts_with("./"))
            });
            let mut first_unrun = 0;
            if has_local_actions {
                if !first_is_checkout {
                    self.line(
                        "##[error]local actions require actions/checkout as the first step so they can be preflighted",
                    )
                    .await?;
                    self.failed = true;
                } else if let Err(e) = self.run_steps_from(&steps[..1], &mut scope, true, 0).await {
                    self.line(&format!("##[error]{}", e.message)).await?;
                    self.failed = true;
                } else if !self.failed {
                    first_unrun = 1;
                    match super::actions::local_container_request(&self, &steps) {
                        Ok(Some(what)) => {
                            self.line(&format!(
                                "##[error]this runner cannot run containers: {what}"
                            ))
                            .await?;
                            self.failed = true;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            self.line(&format!("##[error]{}", e.message)).await?;
                            self.failed = true;
                        }
                    }
                }
            }
            let remaining = &steps[first_unrun..];
            if !self.failed
                && let Err(e) = super::actions::run_pres(&mut self, remaining, &scope).await
            {
                self.line(&format!("##[error]{}", e.message)).await?;
                self.failed = true;
            }
            let res = if self.failed {
                Ok(())
            } else {
                self.run_steps_from(remaining, &mut scope, true, first_unrun)
                    .await
            };
            if let Err(e) = res {
                if self.lost.load(Ordering::SeqCst)
                    || e.reason == bee_proto::v1::ErrorReason::InvalidState
                {
                    heartbeat.abort();
                    self.cleanup();
                    return Ok(None);
                }
                self.line(&format!("##[error]{}", e.message)).await.ok();
                self.failed = true;
            }
            let posts = std::mem::take(&mut self.post);
            for p in posts.into_iter().rev() {
                if let Err(e) = super::actions::run_post(&mut self, p).await {
                    self.line(&format!("##[warning]{}", e.message)).await?;
                }
            }
        }
        heartbeat.abort();
        if self.lost.load(Ordering::SeqCst) {
            self.cleanup();
            return Ok(None);
        }
        let ctx = self.ctx(&scope, &BTreeMap::new());
        let outputs: Vec<KeyValue> = pairs(&self.plan["outputs"])
            .into_iter()
            .map(|(k, v)| KeyValue {
                key: k,
                value: v.evaluate_string(&ctx).unwrap_or_default(),
            })
            .collect();
        let conclusion = if self.cancel.load(Ordering::SeqCst) || self.force.load(Ordering::SeqCst)
        {
            WorkflowConclusion::Cancelled
        } else if self.timed_out {
            WorkflowConclusion::TimedOut
        } else if self.failed {
            WorkflowConclusion::Failure
        } else {
            WorkflowConclusion::Success
        };
        self.line(&format!(
            "Job finished: {}",
            crate::ci::convert::result_str(conclusion)
        ))
        .await?;
        if let Err(e) = self.log.flush().await {
            if e.reason == bee_proto::v1::ErrorReason::InvalidState {
                self.cleanup();
                return Ok(None);
            }
            return Err(e);
        }
        let res = self
            .api
            .complete(CompleteRunnerJobRequest {
                job_id: self.a.job_id.clone(),
                lease_id: self.a.lease_id.clone(),
                conclusion: conclusion as i32,
                outputs,
                summary_markdown: self.summary.clone(),
                completed_at: Some(ts(now_ms())),
                ..Default::default()
            })
            .await;
        self.cleanup();
        match res {
            Ok(()) => Ok(Some(conclusion)),
            Err(e) if e.reason == bee_proto::v1::ErrorReason::InvalidState => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn cleanup(&self) {
        if !self.config.keep_workspace {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Returns a container request that must be rejected before any step is run.
    fn container_request(&self) -> Option<String> {
        if !self.plan["container"].is_null() {
            return Some("job container".into());
        }
        if self.plan["services"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
        {
            return Some("service container".into());
        }
        self.plan["steps"].as_array().and_then(|steps| {
            steps.iter().find_map(|step| {
                step["action"]
                    .as_str()
                    .filter(|action| action.starts_with("docker://"))
                    .map(|action| action.to_string())
            })
        })
    }
}

/// The `steps` and `inputs` a list of steps sees (a job, or one composite action).
#[derive(Clone, Debug, Default)]
pub struct Scope {
    /// `steps.<id>`.
    pub steps: Object,
    /// `inputs` (composite actions).
    pub inputs: Option<Value>,
}

/// Kills a step's process and, on Unix, its whole process group (the shell's children hold
/// the output pipes).
///
/// The `--` is required: without it procps-ng's `kill` (Linux) exits 0 and signals nothing
/// when the first operand is a negative process-group id, so a timed-out or cancelled step's
/// children kept running and the runner waited for them to finish.
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
}

/// `runner.os`.
pub fn runner_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        _ => "Linux",
    }
}

/// `runner.arch`.
pub fn runner_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "ARM64",
        "arm" => "ARM",
        "x86" => "X86",
        _ => "X64",
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_files_parse_heredocs() {
        let v = parse_kv_file("a=1\nb<<EOF\nx\ny\nEOF\nc=z=w\n");
        assert_eq!(
            v,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "x\ny".into()),
                ("c".into(), "z=w".into())
            ]
        );
    }

    #[test]
    fn commands_split_like_a_shell() {
        assert_eq!(
            split_command("bash --noprofile -eo pipefail {0}"),
            vec!["bash", "--noprofile", "-eo", "pipefail", "{0}"]
        );
        assert_eq!(
            split_command("pwsh -command \". '{0}'\""),
            vec!["pwsh", "-command", ". '{0}'"]
        );
    }
}
