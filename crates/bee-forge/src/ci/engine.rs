//! [`Ci`]: runs and jobs.
//!
//! # Run lifecycle
//!
//! `REQUESTED` (created from a triggering event) → `ACTION_REQUIRED` (held for approval) or
//! `PENDING` (waiting on its concurrency group) → `QUEUED` → `IN_PROGRESS` (a runner took the
//! first job) → `COMPLETED`. A run stores the event, the workflow sources it uses and the
//! `vars` it saw, so every later planning step is a pure function of stored data: jobs are
//! expanded lazily, when their needs finish, with `bee_workflow::plan_job`, and the job graph
//! is rebuilt from the stored job records on every step ([`Ci::advance`]).
//!
//! # Job lifecycle
//!
//! A ready job becomes one record per matrix leg. A leg is held while `max-parallel` legs run
//! or while its job concurrency group is busy (`PENDING`), waits on environment protection
//! (`WAITING`), then is `QUEUED` for a runner whose labels match `runs-on`. A runner takes it
//! with a lease it renews ([`crate::ci::runners`]); a lost lease requeues the job up to
//! [`crate::ci::CiConfig::max_requeues`] times; the timeout fails it as `timed_out`.
//! Reusable-workflow calls are graph scopes of their own whose result becomes the calling
//! job's outcome.
//!
//! # Side effects after commit
//!
//! A transaction collects [`Effects`]: runs to advance, runs and jobs to cancel (concurrency),
//! runs whose check runs need syncing, and whether runners should be woken. They run after
//! commit; every one is idempotent and the maintenance tick repeats what a crash interrupted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_auth::{Attenuation, MintRequest, Scope};
use bee_proto::v1::{
    CheckOutput, CheckStatus, ErrorReason, Event, KeyValue, ObjectId, Principal, Repo, RepoRef,
    RepoRole, SubjectKind, SubjectRef, WorkflowConclusion, WorkflowJob, WorkflowRun,
    WorkflowRunConcurrency, WorkflowState, WorkflowStatus, repo_ref,
};
use bee_workflow::concurrency::{Member, MemberState, Request as ConcRequest, admit};
use bee_workflow::event::{BeeEvent, Cause, WorkflowRunEvent, triggers};
use bee_workflow::graph::{Decision, JobOutcome, JobResult, LegOutcome, RunGraph, aggregate_legs};
use bee_workflow::model::Access;
use bee_workflow::model::{JobKind, WorkflowRef};
use bee_workflow::plan::{
    CallSecretsPlan, CalledWorkflow, JobBody, PlannedJob, RunInfo, WorkflowResolver,
    github_context, plan_job, plan_workflow,
};
use bee_workflow::value::Object;
use bee_workflow::{ActionsEvent, PlanInput, RepoInfo, Value, Workflow, map_event, parse_workflow};
use tokio::sync::{Mutex as AsyncMutex, Notify};

use super::convert::{
    PlanFacts, check_status, conclusion_of, effective_permissions, git_ref, load_event, load_repo,
    permission_strings, plan_json, result_of, result_str, store_event, store_repo, template_json,
    token_actions,
};
use super::model::{
    ConcurrencyGroup, ConcurrencyMember, JobIndex, JobRecord, QueueEntry, RunRecord, RunnerRecord,
    ScopeRecord, UsageRecord, WorkflowRow,
};
use super::ports::{CiBlobs, CiConfig, KeyPort, SecretCipher, UrlSigner};
use super::sealed::{DerivedSealKeys, SealedBoxPort};
use super::util::{json_object, object_json};
use super::vcs::CiVcs;
use super::{
    ATTEMPTS, CONCURRENCY, COUNTERS, JOB_INDEX, JOBS, QUEUE, RUNNERS, RUNS, SURFACE, USAGE,
    WORKFLOWS,
};
use crate::audit::AuditDraft;
use crate::checks::{Checks, Report, RunPatch};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::{hex, ts, ts_ms, unhex};
use crate::orgs::{AccessSource, Orgs};
use crate::store::{Db, Key, Row, Scan, Tx};

/// Directories workflows are read from.
pub const WORKFLOW_DIRS: [&str; 2] = [".github/workflows", ".beehive/workflows"];

/// Most jobs (all attempts) one run may have.
pub const MAX_JOBS_PER_RUN: usize = 5000;

/// Whether `path` is a workflow file: a `.yml`/`.yaml` file directly in a workflow directory.
pub fn is_ci_workflow_path(path: &str) -> bool {
    WORKFLOW_DIRS.iter().any(|d| {
        path.strip_prefix(d)
            .and_then(|r| r.strip_prefix('/'))
            .is_some_and(|f| !f.contains('/') && (f.ends_with(".yml") || f.ends_with(".yaml")))
    })
}

/// What CI needs besides the forge.
pub struct CiHooks {
    /// VCS reads.
    pub vcs: Arc<dyn CiVcs>,
    /// Blob storage for logs, artifacts and caches.
    pub blobs: Arc<dyn CiBlobs>,
    /// Signing key for OIDC tokens and attestations.
    pub keys: Arc<dyn KeyPort>,
    /// Server key sealing secrets at rest.
    pub secret_key: [u8; 32],
    /// Key for signed transfer URLs.
    pub url_key: Vec<u8>,
    /// The token signer (the server's key ring) for job tokens and runner credentials.
    pub signer: Arc<dyn crate::TokenIssuer>,
    /// Settings.
    pub config: CiConfig,
}

/// Facts about a trigger that the event itself does not carry.
#[derive(Clone, Debug, Default)]
pub struct TriggerContext {
    /// Change request numbers the run checks.
    pub change_request_numbers: Vec<u64>,
    /// Change request ids the run checks.
    pub change_request_ids: Vec<String>,
    /// Head ref (bee name) shown on the run.
    pub head_ref: String,
    /// Base ref (bee name) of a change request.
    pub base_ref: String,
    /// The changeset check runs are reported on (the CR head for change request events).
    pub check_changeset: Option<ObjectId>,
    /// The change request was authored by an agent.
    pub agent_author: bool,
    /// The change request comes from a fork.
    pub from_fork: bool,
    /// The change request's author (first-time contributor rules of fork runs).
    pub author: Option<Principal>,
    /// Who caused the event.
    pub actor: Option<Principal>,
    /// `workflow_run` chain depth of the triggering run.
    pub chain_depth: u32,
    /// Non-empty for a bisect probe: the top-level job keys to run (see
    /// [`Ci::start_probe_run`]).
    pub probe_jobs: Vec<String>,
}

/// Work to do after a transaction commits.
#[derive(Default, Debug)]
pub struct Effects {
    /// Runs to advance: (repo id, run id).
    pub advance: BTreeSet<(String, String)>,
    /// Runs to cancel: (repo id, run id).
    pub cancel_runs: BTreeSet<(String, String)>,
    /// Runs to start after a concurrency group freed up.
    pub start_runs: BTreeSet<(String, String)>,
    /// Runs whose check runs need syncing.
    pub sync: BTreeSet<(String, String)>,
    /// Completed runs that may trigger `workflow_run` workflows.
    pub completed: Vec<(String, String)>,
    /// Wake waiting runners.
    pub wake: bool,
}

/// Status helpers.
pub(crate) fn status_of(j: &WorkflowJob) -> WorkflowStatus {
    WorkflowStatus::try_from(j.status).unwrap_or(WorkflowStatus::Unspecified)
}

pub(crate) fn run_status(r: &WorkflowRun) -> WorkflowStatus {
    WorkflowStatus::try_from(r.status).unwrap_or(WorkflowStatus::Unspecified)
}

pub(crate) fn job_of(r: &JobRecord) -> &WorkflowJob {
    static EMPTY: std::sync::OnceLock<WorkflowJob> = std::sync::OnceLock::new();
    r.job
        .as_ref()
        .unwrap_or_else(|| EMPTY.get_or_init(WorkflowJob::default))
}

pub(crate) fn run_of(r: &RunRecord) -> &WorkflowRun {
    static EMPTY: std::sync::OnceLock<WorkflowRun> = std::sync::OnceLock::new();
    r.run
        .as_ref()
        .unwrap_or_else(|| EMPTY.get_or_init(WorkflowRun::default))
}

/// A run as served: the stored run with its numeric id.
pub(crate) fn served_run(r: &RunRecord) -> WorkflowRun {
    let mut run = run_of(r).clone();
    run.numeric_id = r.numeric_id;
    run
}

pub(crate) fn repo_ref_of(id: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    })
}

pub(crate) fn object_id(hexs: &str) -> Option<ObjectId> {
    unhex(hexs)
        .filter(|b| !b.is_empty())
        .map(|raw| ObjectId { raw })
}

fn file_map(kv: &[KeyValue]) -> BTreeMap<String, String> {
    kv.iter()
        .map(|p| (p.key.clone(), p.value.clone()))
        .collect()
}

struct MapResolver<'a>(&'a BTreeMap<String, String>);

impl WorkflowResolver for MapResolver<'_> {
    fn resolve(&self, owner: &str, repo: &str, path: &str, git_ref: &str) -> Option<String> {
        self.0.get(&remote_key(owner, repo, path, git_ref)).cloned()
    }
}

fn remote_key(owner: &str, repo: &str, path: &str, git_ref: &str) -> String {
    format!("{owner}/{repo}/{path}@{git_ref}")
}

fn workflow_key(r: &WorkflowRef) -> String {
    match r {
        WorkflowRef::Local(p) => p.trim_start_matches("./").to_string(),
        WorkflowRef::Remote {
            owner,
            repo,
            path,
            git_ref,
        } => remote_key(owner, repo, path, git_ref),
    }
}

/// Everything needed to call the planner for a stored run.
struct Planning {
    info: RepoInfo,
    files: BTreeMap<String, String>,
    event: ActionsEvent,
    run: RunInfo,
    vars: Object,
    schedule: Option<String>,
    top_key: String,
}

impl Planning {
    fn of(rec: &RunRecord) -> Planning {
        let run = run_of(rec);
        Planning {
            info: load_repo(&rec.repo_info.clone().unwrap_or_default()),
            files: file_map(&rec.files),
            event: load_event(&rec.event.clone().unwrap_or_default()),
            run: RunInfo {
                run_id: rec.numeric_id,
                run_number: run.run_number,
                run_attempt: run.run_attempt.max(1),
            },
            vars: json_object(&rec.vars_json),
            schedule: (!rec.schedule.is_empty()).then(|| rec.schedule.clone()),
            top_key: rec.workflow_key.clone(),
        }
    }

    fn input(&self) -> PlanInput<'_> {
        PlanInput {
            repo: &self.info,
            files: &self.files,
            event: &self.event,
            run: self.run.clone(),
            vars: self.vars.clone(),
            resolver: None,
        }
    }

    fn workflow(&self, key: &str) -> Result<Workflow> {
        let src = self
            .files
            .get(key)
            .ok_or_else(|| ForgeError::internal(format!("workflow {key} missing from the run")))?;
        parse_workflow(key, src).map_err(|e| {
            ForgeError::internal(format!("stored workflow {key} no longer parses: {e}"))
        })
    }

    fn github(&self, wf: &Workflow) -> Value {
        github_context(
            &self.event,
            &self.info,
            &self.run,
            &wf.path,
            wf.display_name(),
        )
    }
}

/// The CI domain.
pub struct Ci {
    pub(crate) db: Db,
    pub(crate) orgs: Arc<Orgs>,
    pub(crate) checks: Arc<Checks>,
    pub(crate) vcs: Arc<dyn CiVcs>,
    pub(crate) blobs: Arc<dyn CiBlobs>,
    pub(crate) keys: Arc<dyn KeyPort>,
    pub(crate) cipher: SecretCipher,
    pub(crate) urls: UrlSigner,
    pub(crate) signer: Arc<dyn crate::TokenIssuer>,
    pub(crate) config: CiConfig,
    pub(crate) wake: Arc<Notify>,
    sealed: std::sync::RwLock<Arc<dyn SealedBoxPort>>,
    fork_gate: std::sync::RwLock<Option<Arc<dyn ForkRunGate>>>,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

/// Decides whether workflow runs of a change request from a fork wait for a maintainer's
/// approval before they start (first-time contributor rules). The forks module answers over
/// enterprise moderation (`crate::forks::ModerationGate`); without a gate every fork run
/// under a first-time policy waits.
#[async_trait]
pub trait ForkRunGate: Send + Sync {
    /// Whether runs of `author`'s fork change request in `repo` need approval.
    async fn needs_approval(&self, repo: &Repo, author: &Principal) -> Result<bool>;
}

impl Ci {
    /// CI over `forge` with `hooks`.
    pub fn new(forge: &Forge, hooks: CiHooks) -> Arc<Ci> {
        let sealed: Arc<dyn SealedBoxPort> = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(b"swarm.ci.sealed-box.master\n");
            h.update(hooks.secret_key);
            Arc::new(DerivedSealKeys::new(h.finalize().into()))
        };
        Arc::new(Ci {
            db: forge.db.clone(),
            orgs: forge.orgs.clone(),
            checks: forge.checks.clone(),
            vcs: hooks.vcs,
            blobs: hooks.blobs,
            keys: hooks.keys,
            cipher: SecretCipher::new(hooks.secret_key),
            urls: UrlSigner::new(hooks.config.server_url.clone(), &hooks.url_key),
            signer: hooks.signer,
            config: hooks.config,
            wake: Arc::new(Notify::new()),
            sealed: std::sync::RwLock::new(sealed),
            fork_gate: std::sync::RwLock::new(None),
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// Installs the gate deciding which fork change requests' runs wait for approval.
    pub fn set_fork_gate(&self, gate: Arc<dyn ForkRunGate>) {
        if let Ok(mut g) = self.fork_gate.write() {
            *g = Some(gate);
        }
    }

    /// Whether runs of a fork change request by `author` wait for approval under the
    /// actions policy's `fork_approval`: always for `ALL_EXTERNAL`, else as the gate decides
    /// (first-time contributors, new accounts per the moderation settings). Without a gate
    /// or an author they wait.
    pub(crate) async fn fork_run_needs_approval(
        &self,
        repo: &Repo,
        author: Option<&Principal>,
        fork_approval: i32,
    ) -> bool {
        use bee_proto::v1::ActionsPolicyForkApproval as F;
        if fork_approval == F::AllExternal as i32 {
            return true;
        }
        let gate = self.fork_gate.read().ok().and_then(|g| g.clone());
        match (gate, author) {
            (Some(g), Some(a)) => g.needs_approval(repo, a).await.unwrap_or(true),
            _ => true,
        }
    }

    /// Replaces the sealed-box key port (`SecretService` public keys and `PutSecret`). The
    /// default derives per-scope X25519 keys from the server's secret key
    /// ([`DerivedSealKeys`]); a deployment with a KMS wires its own [`SealedBoxPort`].
    pub fn set_sealed_box_port(&self, port: Arc<dyn SealedBoxPort>) {
        if let Ok(mut g) = self.sealed.write() {
            *g = port;
        }
    }

    /// The sealed-box key port.
    pub fn sealed_box(&self) -> Arc<dyn SealedBoxPort> {
        self.sealed
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Registers the event listeners (triggers, `green/*`) and the job-token access source
    /// with `forge`.
    pub fn attach(self: &Arc<Self>, forge: &Forge) {
        forge
            .events
            .add_listener(Arc::new(super::trigger::CiListener::with_registry(
                self.clone(),
                forge.registry.clone(),
            )));
        forge
            .events
            .add_listener(Arc::new(super::green::GreenListener::new(self.clone())));
        forge.orgs.add_access_source(Arc::new(JobAccess {
            db: self.db.clone(),
        }));
    }

    /// Starts the CI worker: the maintenance tick every `interval` and the schedule tick
    /// once per UTC minute of the injected clock. Stop it with the returned sender.
    pub fn spawn_worker(
        self: &Arc<Self>,
        interval: std::time::Duration,
    ) -> (
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let me = self.clone();
        let task = tokio::spawn(async move {
            loop {
                if *stop.borrow() {
                    return;
                }
                let cx = Cx::system();
                let _ = me.tick(&cx).await;
                let minute = super::convert::utc_minute(me.now());
                let _ = me.tick_schedule(&cx, minute).await;
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = stop.changed() => {}
                }
            }
        });
        (stop_tx, task)
    }

    /// Settings.
    pub fn config(&self) -> &CiConfig {
        &self.config
    }

    /// The URL signer (the server's transfer endpoints verify with it).
    pub fn url_signer(&self) -> &UrlSigner {
        &self.urls
    }

    /// Signalled whenever a job is queued or a lease changes.
    pub fn wake_signal(&self) -> &Arc<Notify> {
        &self.wake
    }

    pub(crate) fn now(&self) -> u64 {
        self.db.now_ms()
    }

    fn lock(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut m = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(key.to_string()).or_default().clone()
    }

    pub(crate) async fn repo_by_id(&self, cx: &Cx, id: &str) -> Result<Repo> {
        self.orgs.repo_any(cx, id).await
    }

    /// `RepoInfo` for planning.
    pub(crate) async fn repo_info(&self, repo: &Repo) -> Result<RepoInfo> {
        let default = if repo.default_ref.is_empty() {
            "main".to_string()
        } else {
            super::convert::bee_ref(&repo.default_ref)
        };
        let tip = self.vcs.ref_tip(repo, &default).await?;
        let (owner, name) = repo
            .full_name
            .split_once('/')
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .unwrap_or_else(|| (repo.org_id.clone(), repo.name.clone()));
        Ok(RepoInfo {
            owner,
            name,
            default_branch: default,
            default_branch_sha: tip.map(|t| hex(&t.raw)).unwrap_or_default(),
            server_url: self.config.server_url.clone(),
            api_url: self.config.api_url.clone(),
            graphql_url: self.config.graphql_url.clone(),
        })
    }

    /// Workflow files at a changeset: path → YAML.
    pub async fn workflow_files(
        &self,
        repo: &Repo,
        at: &ObjectId,
    ) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for dir in WORKFLOW_DIRS {
            for name in self.vcs.list_dir(repo, at, dir).await? {
                let path = format!("{dir}/{name}");
                if !is_ci_workflow_path(&path) {
                    continue;
                }
                if let Some(bytes) = self.vcs.read_file(repo, at, &path).await? {
                    out.insert(path, String::from_utf8_lossy(&bytes).into_owned());
                }
            }
        }
        Ok(out)
    }

    /// Loads remote reusable workflows (`owner/repo/path@ref`) the sources call, transitively,
    /// into `files` under their remote keys.
    pub(crate) async fn prefetch_remote(&self, cx: &Cx, files: &mut BTreeMap<String, String>) {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for _ in 0..MAX_LEVELS {
            let mut wanted = Vec::new();
            for (path, src) in files.iter() {
                let Ok(wf) = parse_workflow(path, src) else {
                    continue;
                };
                for j in &wf.jobs {
                    if let JobKind::Call(c) = &j.kind
                        && let WorkflowRef::Remote { .. } = &c.uses
                    {
                        let k = workflow_key(&c.uses);
                        if !files.contains_key(&k) && seen.insert(k) {
                            wanted.push(c.uses.clone());
                        }
                    }
                }
            }
            if wanted.is_empty() {
                return;
            }
            for w in wanted {
                let WorkflowRef::Remote {
                    owner,
                    repo,
                    path,
                    git_ref: r,
                } = &w
                else {
                    continue;
                };
                let full = format!("{owner}/{repo}");
                let Ok(other) = self
                    .orgs
                    .resolve_repo(
                        cx,
                        &RepoRef {
                            repo: Some(repo_ref::Repo::FullName(full)),
                        },
                    )
                    .await
                else {
                    continue;
                };
                let at = match object_id(r).filter(|o| o.raw.len() > 8) {
                    Some(id) => Some(id),
                    None => self
                        .vcs
                        .ref_tip(&other, &super::convert::bee_ref(r))
                        .await
                        .ok()
                        .flatten(),
                };
                let Some(at) = at else { continue };
                if let Ok(Some(b)) = self.vcs.read_file(&other, &at, path).await {
                    files.insert(workflow_key(&w), String::from_utf8_lossy(&b).into_owned());
                }
            }
        }
    }

    /// Records a workflow seen at `at`, returning its row.
    pub(crate) async fn ensure_workflow(
        &self,
        cx: &Cx,
        repo: &Repo,
        path: &str,
        wf: Option<&Workflow>,
        at: Option<&ObjectId>,
        git: &str,
    ) -> Result<WorkflowRow> {
        let blob = match at {
            Some(a) => self.vcs.file_id(repo, a, path).await?,
            None => None,
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let existing = tx
            .scan(
                &WORKFLOWS,
                &Scan::scope(repo.id.clone(), 1).key(Key::K1(path.to_string())),
            )
            .await?;
        let now = tx.now_ms();
        let (mut row, version, id) = match existing.first() {
            Some(r) => (r.decode::<WorkflowRow>()?, r.version, r.id.clone()),
            None => (WorkflowRow::default(), 0, tx.new_id()?),
        };
        let mut w = row.workflow.clone().unwrap_or_default();
        let fresh = version == 0;
        if fresh {
            w.id = id.clone();
            w.repo = repo_ref_of(&repo.id);
            w.path = path.to_string();
            w.state = WorkflowState::Active as i32;
            w.created_at = Some(ts(now));
            w.badge_url = format!(
                "{}/{}/actions/workflows/{}/badge.svg",
                self.config.server_url,
                repo.full_name,
                path.rsplit('/').next().unwrap_or(path)
            );
        }
        let before = w.clone();
        if let Some(wf) = wf {
            w.name = wf.display_name().to_string();
            w.triggers = wf.on.iter().map(|t| t.name.clone()).collect();
            w.dispatch_inputs = wf
                .trigger("workflow_dispatch")
                .map(|t| {
                    t.inputs
                        .iter()
                        .map(|i| bee_proto::v1::WorkflowDispatchInput {
                            name: i.id.clone(),
                            description: i.description.clone().unwrap_or_default(),
                            r#type: i.kind.name().to_string(),
                            required: i.required,
                            default_value: i
                                .default
                                .as_ref()
                                .and_then(|d| d.as_literal_str())
                                .unwrap_or_default(),
                            options: i.options.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            row.schedules = wf
                .trigger("schedule")
                .map(|t| t.schedules.iter().map(|c| c.source().to_string()).collect())
                .unwrap_or_default();
            if w.state == WorkflowState::Deleted as i32 {
                w.state = WorkflowState::Active as i32;
            }
        } else if w.name.is_empty() {
            w.name = path.to_string();
        }
        if !git.is_empty() {
            w.r#ref = git.to_string();
        }
        if blob.is_some() {
            w.blob = blob;
        }
        if !fresh && w == before && wf.is_none() {
            return Ok(row);
        }
        w.updated_at = Some(ts(now));
        row.workflow = Some(w);
        row.org_id = repo.org_id.clone();
        let mut r = Row::new(repo.id.clone(), id.clone())
            .k1(path)
            .version(version)
            .msg(&row);
        if !row.schedules.is_empty() {
            r = r.k2("schedule");
        }
        if fresh {
            tx.insert(&WORKFLOWS, &r).await?;
        } else {
            tx.update(&WORKFLOWS, &r).await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    async fn counter(&self, cx: &Cx, scope: &str, id: &str) -> Result<u64> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let n = tx.add_n1(&COUNTERS, scope, id, 1).await?;
        tx.commit().await?;
        Ok(n as u64)
    }

    // ----- triggering -------------------------------------------------------------------------

    /// Starts every workflow `event` triggers in `repo`; returns the new run ids.
    pub async fn trigger(
        &self,
        cx: &Cx,
        repo: &Repo,
        event: &BeeEvent,
        cause: &Cause,
        ctx: &TriggerContext,
    ) -> Result<Vec<String>> {
        let policy = self.effective_policy(cx, repo).await?;
        if !policy.enabled {
            return Ok(Vec::new());
        }
        let info = self.repo_info(repo).await?;
        let dispatch_target = match event {
            BeeEvent::WorkflowDispatch { workflow, .. } => Some(workflow.clone()),
            _ => None,
        };
        let mut out = Vec::new();
        let mut snapshots: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for ae in map_event(event, cause, &info) {
            let Some(sha) = object_id(&ae.sha) else {
                continue;
            };
            if !snapshots.contains_key(&ae.sha) {
                let files = self.workflow_files(repo, &sha).await?;
                snapshots.insert(ae.sha.clone(), files);
            }
            let files = snapshots.get(&ae.sha).cloned().unwrap_or_default();
            for (path, src) in &files {
                if dispatch_target.as_ref().is_some_and(|t| t != path) {
                    continue;
                }
                match parse_workflow(path, src) {
                    Err(e) => {
                        if matches!(ae.name.as_str(), "push" | "pull_request") {
                            let id = self
                                .startup_failure(
                                    cx,
                                    repo,
                                    &info,
                                    &ae,
                                    path,
                                    None,
                                    &files,
                                    ctx,
                                    &e.to_string(),
                                )
                                .await?;
                            out.extend(id);
                        }
                    }
                    Ok(wf) => {
                        let Ok(hits) = triggers(&wf, &ae) else {
                            continue;
                        };
                        for hit in hits {
                            if let Some(id) = self
                                .create_run(cx, repo, &info, &ae, &wf, &hit, &files, ctx, None)
                                .await?
                            {
                                out.push(id);
                            }
                        }
                    }
                }
            }
            if matches!(ae.name.as_str(), "pull_request" | "pull_request_target") {
                for (req_id, src_path, src) in self.required_sources(cx, repo).await? {
                    let mut files = files.clone();
                    let key = format!(
                        ".beehive/required/{req_id}/{}",
                        src_path.rsplit('/').next().unwrap_or("workflow.yml")
                    );
                    files.insert(key.clone(), src.clone());
                    let Ok(wf) = parse_workflow(&key, &src) else {
                        continue;
                    };
                    let Ok(hits) = triggers(&wf, &ae) else {
                        continue;
                    };
                    for hit in hits {
                        if let Some(id) = self
                            .create_run(cx, repo, &info, &ae, &wf, &hit, &files, ctx, Some(&req_id))
                            .await?
                        {
                            out.push(id);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn base_run(
        &self,
        repo: &Repo,
        ae: &ActionsEvent,
        ctx: &TriggerContext,
        cx: &Cx,
        now: u64,
    ) -> WorkflowRun {
        let actor = ctx.actor.clone().unwrap_or_else(|| cx.principal());
        WorkflowRun {
            repo: repo_ref_of(&repo.id),
            event: ae.name.clone(),
            status: WorkflowStatus::Requested as i32,
            head_ref: if ctx.head_ref.is_empty() {
                super::convert::bee_ref(&ae.git_ref)
            } else {
                ctx.head_ref.clone()
            },
            head_changeset: object_id(&ae.sha),
            head_repo: repo_ref_of(&repo.id),
            change_request_numbers: ctx.change_request_numbers.clone(),
            change_request_ids: ctx.change_request_ids.clone(),
            actor: Some(actor.clone()),
            triggering_actor: Some(actor),
            run_attempt: 1,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn startup_failure(
        &self,
        cx: &Cx,
        repo: &Repo,
        info: &RepoInfo,
        ae: &ActionsEvent,
        path: &str,
        wf: Option<&Workflow>,
        files: &BTreeMap<String, String>,
        ctx: &TriggerContext,
        error: &str,
    ) -> Result<Option<String>> {
        let row = self
            .ensure_workflow(cx, repo, path, wf, object_id(&ae.sha).as_ref(), &ae.git_ref)
            .await?;
        let w = row.workflow.clone().unwrap_or_default();
        if w.state != WorkflowState::Active as i32 {
            return Ok(None);
        }
        let number = self
            .counter(cx, &repo.id, &format!("run_number:{}", w.id))
            .await?;
        let numeric = self.counter(cx, "", "run_id").await?;
        let now = self.now();
        let mut run = self.base_run(repo, ae, ctx, cx, now);
        run.workflow_id = w.id.clone();
        run.workflow_path = path.to_string();
        run.name = w.name.clone();
        run.display_title = w.name.clone();
        run.run_number = number;
        run.status = WorkflowStatus::Completed as i32;
        run.conclusion = WorkflowConclusion::StartupFailure as i32;
        run.completed_at = Some(ts(now));
        let mut tx = self.db.begin(SURFACE, cx).await?;
        run.id = tx.new_id()?;
        run.html_url = self.run_url(repo, &run.id);
        run.numeric_id = numeric;
        let rec = RunRecord {
            run: Some(run.clone()),
            event: Some(store_event(ae)),
            files: super::convert::to_kv(files),
            numeric_id: numeric,
            org_id: repo.org_id.clone(),
            repo_info: Some(store_repo(info)),
            startup_error: error.to_string(),
            workflow_key: path.to_string(),
            ..Default::default()
        };
        tx.insert(&RUNS, &run_row(&repo.id, &rec)).await?;
        self.emit_run(&mut tx, &rec, "workflow_run.completed")
            .await?;
        tx.audit(
            AuditDraft::new("workflow.run_failed_to_start", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::WorkflowRun, &run.id)
                .after(&serde_json::json!({ "workflow": path, "error": error }).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(run.id))
    }

    pub(crate) fn run_url(&self, repo: &Repo, run_id: &str) -> String {
        format!(
            "{}/{}/actions/runs/{run_id}",
            self.config.server_url, repo.full_name
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_run(
        &self,
        cx: &Cx,
        repo: &Repo,
        info: &RepoInfo,
        ae: &ActionsEvent,
        wf: &Workflow,
        hit: &bee_workflow::event::Triggered,
        files: &BTreeMap<String, String>,
        ctx: &TriggerContext,
        required: Option<&str>,
    ) -> Result<Option<String>> {
        let row = self
            .ensure_workflow(
                cx,
                repo,
                &wf.path,
                Some(wf),
                object_id(&ae.sha).as_ref(),
                &ae.git_ref,
            )
            .await?;
        let w = row.workflow.clone().unwrap_or_default();
        if w.state != WorkflowState::Active as i32 {
            return Ok(None);
        }
        let mut files = files.clone();
        self.prefetch_remote(cx, &mut files).await;
        let vars = self.vars_for(cx, repo, "").await?;
        let vars_obj: Object = vars
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect();
        let number = self
            .counter(cx, &repo.id, &format!("run_number:{}", w.id))
            .await?;
        let numeric = self.counter(cx, "", "run_id").await?;
        let policy = self.effective_policy(cx, repo).await?;
        let planned = {
            let resolver = MapResolver(&files);
            let input = PlanInput {
                repo: info,
                files: &files,
                event: ae,
                run: RunInfo {
                    run_id: numeric,
                    run_number: number,
                    run_attempt: 1,
                },
                vars: vars_obj.clone(),
                resolver: Some(&resolver),
            };
            plan_workflow(wf, hit, &input)
        };
        let planned = match planned {
            Ok(p) => match self.check_actions_policy(&policy, repo, &p) {
                Ok(()) => p,
                Err(msg) => {
                    return self
                        .startup_failure(cx, repo, info, ae, &wf.path, Some(wf), &files, ctx, &msg)
                        .await;
                }
            },
            Err(e) => {
                return self
                    .startup_failure(
                        cx,
                        repo,
                        info,
                        ae,
                        &wf.path,
                        Some(wf),
                        &files,
                        ctx,
                        &e.to_string(),
                    )
                    .await;
            }
        };
        let probe = !ctx.probe_jobs.is_empty();
        let fork_wait = ctx.from_fork
            && !probe
            && self
                .fork_run_needs_approval(repo, ctx.author.as_ref(), policy.fork_approval)
                .await;
        let approval =
            !probe && ((ctx.agent_author && policy.agent_runs_require_approval) || fork_wait);
        let now = self.now();
        let mut run = self.base_run(repo, ae, ctx, cx, now);
        run.workflow_id = w.id.clone();
        run.workflow_path = wf.path.clone();
        run.name = planned.name.clone();
        run.display_title = planned.run_name.clone();
        run.run_number = number;
        run.approval_required = approval;
        if let Some(c) = planned.concurrency.as_ref().filter(|_| !probe) {
            run.concurrency = Some(WorkflowRunConcurrency {
                group: c.group.clone(),
                cancel_in_progress: c.cancel_in_progress,
                ..Default::default()
            });
        }
        run.referenced_workflows = files
            .keys()
            .filter(|k| k.contains('@'))
            .map(|k| {
                let (p, r) = k.rsplit_once('@').unwrap_or((k, ""));
                bee_proto::v1::WorkflowReference {
                    path: p.to_string(),
                    r#ref: r.to_string(),
                    changeset: None,
                }
            })
            .collect();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        run.id = tx.new_id()?;
        run.html_url = self.run_url(repo, &run.id);
        run.numeric_id = numeric;
        let rec = RunRecord {
            run: Some(run.clone()),
            event: Some(store_event(ae)),
            files: super::convert::to_kv(&files),
            vars_json: object_json(&vars_obj),
            numeric_id: numeric,
            schedule: hit.schedule.clone().unwrap_or_default(),
            org_id: repo.org_id.clone(),
            repo_info: Some(store_repo(info)),
            concurrency_key: planned
                .concurrency
                .as_ref()
                .filter(|_| !probe)
                .map(|c| bee_workflow::concurrency::group_key(&c.group))
                .unwrap_or_default(),
            concurrency_cancel: planned
                .concurrency
                .as_ref()
                .is_some_and(|c| c.cancel_in_progress),
            concurrency_queue_max: planned.concurrency.as_ref().is_some_and(|c| c.queue_max),
            required_workflow_id: required.unwrap_or_default().to_string(),
            check_changeset: if probe {
                String::new()
            } else {
                ctx.check_changeset
                    .as_ref()
                    .map(|c| hex(&c.raw))
                    .unwrap_or_else(|| ae.sha.clone())
            },
            only_jobs: ctx.probe_jobs.clone(),
            probe,
            by_workflow_token: ae.by_workflow_token,
            base_ref: ctx.base_ref.clone(),
            workflow_key: wf.path.clone(),
            chain_depth: ctx.chain_depth,
            repo_public: repo.visibility == bee_proto::v1::Visibility::Public as i32,
            from_fork: ctx.from_fork && !probe,
            ..Default::default()
        };
        tx.insert(&RUNS, &run_row(&repo.id, &rec)).await?;
        self.emit_run(&mut tx, &rec, "workflow_run.requested")
            .await?;
        tx.audit(
            AuditDraft::new("workflow.run_created", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::WorkflowRun, &run.id)
                .after(
                    &serde_json::json!({
                        "workflow": wf.path, "event": ae.name, "sha": ae.sha,
                        "approval_required": approval,
                    })
                    .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        self.start_run(cx, &repo.id, &run.id).await?;
        Ok(Some(run.id))
    }

    /// Starts a bisect probe: the workflow at `workflow_path` as of `changeset`, as if
    /// `changeset` had been pushed to `branch`, running only `job_key` and the jobs it needs
    /// (the rest are skipped). Probe runs report no check runs, join no concurrency group,
    /// need no approval and start no `workflow_run` chain. Returns the run id. Errors:
    /// NOT_FOUND (the workflow or the job does not exist at `changeset`), INVALID_ARGUMENT
    /// (the workflow does not parse there), INVALID_STATE (the workflow is disabled or the plan
    /// failed to start).
    pub async fn start_probe_run(
        &self,
        cx: &Cx,
        repo: &Repo,
        workflow_path: &str,
        job_key: &str,
        changeset: &ObjectId,
        branch: &str,
    ) -> Result<String> {
        let info = self.repo_info(repo).await?;
        let files = self.workflow_files(repo, changeset).await?;
        let src = files.get(workflow_path).ok_or_else(|| {
            ForgeError::not_found(format!(
                "workflow {workflow_path} at {}",
                hex(&changeset.raw)
            ))
        })?;
        let wf = parse_workflow(workflow_path, src)
            .map_err(|e| ForgeError::invalid("invalid_workflow", e.to_string()))?;
        let target = wf.job(job_key).ok_or_else(|| {
            ForgeError::not_found(format!(
                "job {job_key} in {workflow_path} at {}",
                hex(&changeset.raw)
            ))
        })?;
        let mut keep: BTreeSet<String> = BTreeSet::new();
        let mut stack = vec![target.id.clone()];
        while let Some(k) = stack.pop() {
            if keep.insert(k.to_lowercase())
                && let Some(j) = wf.job(&k)
            {
                stack.extend(j.needs.iter().cloned());
            }
        }
        let push = bee_workflow::event::PushEvent {
            git_ref: git_ref(branch),
            before: "0".repeat(64),
            after: hex(&changeset.raw),
            created: false,
            deleted: false,
            forced: false,
            commit_count: 1,
            head_commit_message: self.vcs.message(repo, changeset).await.ok(),
            changed: bee_workflow::event::ChangedPaths::Unknown,
        };
        let cause = Cause {
            actor: cx.actor_id.clone(),
            by_workflow_token: false,
        };
        let mut ae = map_event(&BeeEvent::Push(push), &cause, &info)
            .into_iter()
            .find(|e| e.name == "push")
            .ok_or_else(|| ForgeError::internal("a push maps to a push event"))?;
        ae.skip_ci = false;
        let ctx = TriggerContext {
            head_ref: branch.to_string(),
            actor: Some(cx.principal()),
            probe_jobs: keep.into_iter().collect(),
            ..Default::default()
        };
        let hit = bee_workflow::event::Triggered { schedule: None };
        let id = self
            .create_run(cx, repo, &info, &ae, &wf, &hit, &files, &ctx, None)
            .await?
            .ok_or_else(|| {
                ForgeError::invalid_state(format!("workflow {workflow_path} is not active"))
            })?;
        let rec = self.run_record(cx, &repo.id, &id).await?;
        if !rec.startup_error.is_empty() {
            return Err(ForgeError::invalid_state(format!(
                "the probe failed to start: {}",
                rec.startup_error
            )));
        }
        Ok(id)
    }

    /// The outcome of `job_key` in a probe run (the legs named `check_name` when the job has
    /// several): `None` while the run is unfinished, `Some(None)` when the job did not
    /// produce a verdict (skipped, cancelled, never planned), `Some(Some(passed))` otherwise.
    pub async fn probe_outcome(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        job_key: &str,
        check_name: &str,
    ) -> Result<Option<Option<bool>>> {
        let rec = self.run_record(cx, repo_id, run_id).await?;
        let run = run_of(&rec);
        if run_status(run) != WorkflowStatus::Completed {
            return Ok(None);
        }
        let jobs = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::run_jobs(&mut tx, repo_id, run_id, Some(run.run_attempt)).await?
        };
        let legs: Vec<&JobRecord> = jobs
            .iter()
            .map(|(j, _)| j)
            .filter(|j| j.scope.is_empty() && j.key.eq_ignore_ascii_case(job_key))
            .collect();
        let named: Vec<&JobRecord> = legs
            .iter()
            .copied()
            .filter(|j| job_of(j).name == check_name)
            .collect();
        let legs = if named.is_empty() { legs } else { named };
        if legs.is_empty() {
            return Ok(Some(None));
        }
        let mut passed = true;
        for j in legs {
            match WorkflowConclusion::try_from(job_of(j).conclusion)
                .unwrap_or(WorkflowConclusion::Unspecified)
            {
                WorkflowConclusion::Success | WorkflowConclusion::Neutral => {}
                WorkflowConclusion::Failure
                | WorkflowConclusion::TimedOut
                | WorkflowConclusion::StartupFailure => passed = false,
                _ => return Ok(Some(None)),
            }
        }
        Ok(Some(Some(passed)))
    }

    pub(crate) async fn emit_run(&self, tx: &mut Tx, rec: &RunRecord, kind: &str) -> Result<()> {
        let run = run_of(rec);
        let repo_id = run
            .repo
            .as_ref()
            .and_then(|r| match &r.repo {
                Some(repo_ref::Repo::Id(id)) => Some(id.clone()),
                _ => None,
            })
            .unwrap_or_default();
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: repo_id.clone(),
            org_id: rec.org_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::WorkflowRun as i32,
                repo: repo_ref_of(&repo_id),
                id: run.id.clone(),
            }),
            payload_json: serde_json::to_string(run).unwrap_or_default(),
            ..Default::default()
        })
        .await
    }

    pub(crate) async fn emit_job(
        tx: &mut Tx,
        repo_id: &str,
        rec: &JobRecord,
        kind: &str,
    ) -> Result<()> {
        let job = job_of(rec);
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: repo_id.to_string(),
            org_id: rec.org_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::WorkflowRun as i32,
                repo: repo_ref_of(repo_id),
                id: job.run_id.clone(),
            }),
            payload_json: serde_json::to_string(job).unwrap_or_default(),
            ..Default::default()
        })
        .await
    }

    // ----- starting and advancing runs --------------------------------------------------------

    /// Moves a requested or pending run past approval and concurrency, then advances it.
    pub(crate) async fn start_run(&self, cx: &Cx, repo_id: &str, run_id: &str) -> Result<()> {
        let mut fx = Effects::default();
        {
            let _g = self.lock(run_id).lock_owned().await;
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((mut rec, row)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, run_id).await?
            else {
                return Err(ForgeError::not_found(format!("workflow run {run_id}")));
            };
            let st = run_status(run_of(&rec));
            if !matches!(
                st,
                WorkflowStatus::Requested
                    | WorkflowStatus::Pending
                    | WorkflowStatus::ActionRequired
            ) {
                return Ok(());
            }
            let now = tx.now_ms();
            let mut run = run_of(&rec).clone();
            if run.approval_required {
                if st != WorkflowStatus::ActionRequired {
                    run.status = WorkflowStatus::ActionRequired as i32;
                    run.updated_at = Some(ts(now));
                    rec.run = Some(run);
                    tx.update(&RUNS, &run_row(repo_id, &rec).version(row.version))
                        .await?;
                    tx.commit().await?;
                }
                return Ok(());
            }
            if !rec.concurrency_key.is_empty() && !rec.concurrency_released {
                if st == WorkflowStatus::Pending {
                    return Ok(());
                }
                let start = self
                    .conc_join(
                        &mut tx,
                        repo_id,
                        &rec.concurrency_key,
                        run_id,
                        run_id,
                        rec.concurrency_cancel,
                        rec.concurrency_queue_max,
                        &mut fx,
                    )
                    .await?;
                if !start {
                    run.status = WorkflowStatus::Pending as i32;
                    run.updated_at = Some(ts(now));
                    rec.run = Some(run);
                    tx.update(&RUNS, &run_row(repo_id, &rec).version(row.version))
                        .await?;
                    tx.commit().await?;
                    drop(_g);
                    self.apply(cx, fx).await;
                    return Ok(());
                }
                rec.concurrency_released = true;
            }
            run.status = WorkflowStatus::Queued as i32;
            run.run_started_at = Some(ts(now));
            run.updated_at = Some(ts(now));
            rec.run = Some(run);
            tx.update(&RUNS, &run_row(repo_id, &rec).version(row.version))
                .await?;
            tx.commit().await?;
            fx.advance.insert((repo_id.to_string(), run_id.to_string()));
        }
        self.apply(cx, fx).await;
        Ok(())
    }

    /// Runs post-commit effects. Failures are left for the maintenance tick.
    pub(crate) async fn apply(&self, cx: &Cx, fx: Effects) {
        let mut fx = fx;
        for _ in 0..16 {
            if fx.wake {
                self.wake.notify_waiters();
                fx.wake = false;
            }
            let cancel: Vec<_> = std::mem::take(&mut fx.cancel_runs).into_iter().collect();
            for (repo, run) in cancel {
                if let Ok(more) = self.cancel_inner(cx, &repo, &run, false).await {
                    merge(&mut fx, more);
                }
            }
            let start: Vec<_> = std::mem::take(&mut fx.start_runs).into_iter().collect();
            for (repo, run) in start {
                let _ = Box::pin(self.start_run(cx, &repo, &run)).await;
            }
            let adv: Vec<_> = std::mem::take(&mut fx.advance).into_iter().collect();
            for (repo, run) in &adv {
                match self.advance_inner(cx, repo, run).await {
                    Ok(more) => merge(&mut fx, more),
                    Err(_) => {
                        fx.sync.insert((repo.clone(), run.clone()));
                    }
                }
                fx.sync.insert((repo.clone(), run.clone()));
            }
            let sync: Vec<_> = std::mem::take(&mut fx.sync).into_iter().collect();
            for (repo, run) in sync {
                let _ = self.sync_checks(cx, &repo, &run).await;
            }
            let done = std::mem::take(&mut fx.completed);
            for (repo, run) in done {
                let _ = Box::pin(self.chain_workflow_run(cx, &repo, &run)).await;
            }
            if fx.advance.is_empty()
                && fx.cancel_runs.is_empty()
                && fx.start_runs.is_empty()
                && fx.sync.is_empty()
                && fx.completed.is_empty()
            {
                if fx.wake {
                    self.wake.notify_waiters();
                }
                return;
            }
        }
    }

    /// Advances a run: expands ready jobs, records skipped ones, completes reusable-workflow
    /// calls and the run itself when its graph finishes.
    pub async fn advance(&self, cx: &Cx, repo_id: &str, run_id: &str) -> Result<()> {
        let fx = self.advance_inner(cx, repo_id, run_id).await?;
        self.apply(cx, fx).await;
        self.sync_checks(cx, repo_id, run_id).await
    }

    async fn advance_inner(&self, cx: &Cx, repo_id: &str, run_id: &str) -> Result<Effects> {
        let _g = self.lock(run_id).lock_owned().await;
        let mut fx = Effects::default();
        for attempt in 0..8 {
            match Box::pin(self.advance_once(cx, repo_id, run_id, &mut fx)).await {
                Ok(true) => continue,
                Ok(false) => return Ok(fx),
                Err(e) if e.reason == ErrorReason::VersionMismatch && attempt < 7 => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(fx)
    }

    /// Loads a run's current-attempt jobs in a transaction.
    pub(crate) async fn run_jobs(
        tx: &mut Tx,
        repo_id: &str,
        run_id: &str,
        attempt: Option<u32>,
    ) -> Result<Vec<(JobRecord, Row)>> {
        let rows = tx
            .scan_msgs::<JobRecord>(
                &JOBS,
                &Scan::scope(repo_id.to_string(), MAX_JOBS_PER_RUN)
                    .key(Key::K1(run_id.to_string())),
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter(|(j, _)| attempt.is_none_or(|a| job_of(j).run_attempt == a))
            .collect())
    }

    /// One planning step; returns whether anything changed.
    async fn advance_once(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        fx: &mut Effects,
    ) -> Result<bool> {
        let repo = self.repo_by_id(cx, repo_id).await?;
        let policy = self.effective_policy(cx, &repo).await?;
        let policy_write = policy.default_token_permission
            == bee_proto::v1::ActionsPolicyTokenPermission::Write as i32;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, run_row_v)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, run_id).await?
        else {
            return Err(ForgeError::not_found(format!("workflow run {run_id}")));
        };
        let st = run_status(run_of(&rec));
        if !matches!(st, WorkflowStatus::Queued | WorkflowStatus::InProgress) {
            return Ok(false);
        }
        let attempt = run_of(&rec).run_attempt;
        let mut jobs = Self::run_jobs(&mut tx, repo_id, run_id, Some(attempt)).await?;
        let mut dirty: BTreeSet<usize> = BTreeSet::new();
        let mut new_jobs: Vec<JobRecord> = Vec::new();
        let mut changed = false;
        let now = tx.now_ms();
        let planning = Planning::of(&rec);

        // Matrix housekeeping: fail-fast and max-parallel per (scope, key).
        let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (i, (j, _)) in jobs.iter().enumerate() {
            if j.kind == "step" {
                groups
                    .entry((j.scope.clone(), j.key.clone()))
                    .or_default()
                    .push(i);
            }
        }
        for idxs in groups.values() {
            let failed = idxs.iter().any(|&i| {
                let j = &jobs[i].0;
                status_of(job_of(j)) == WorkflowStatus::Completed
                    && matches!(j.result.as_str(), "failure" | "timed_out")
                    && !j.continue_on_error
                    && j.fail_fast
            });
            for &i in idxs {
                let open = status_of(job_of(&jobs[i].0)) != WorkflowStatus::Completed;
                if failed && open && !jobs[i].0.cancel_requested {
                    let j = &mut jobs[i].0;
                    if status_of(job_of(j)) == WorkflowStatus::InProgress {
                        j.cancel_requested = true;
                    } else {
                        self.finish_job(&mut tx, repo_id, j, WorkflowConclusion::Cancelled, "", fx)
                            .await?;
                    }
                    dirty.insert(i);
                    changed = true;
                }
            }
            if failed {
                continue;
            }
            let max = idxs.first().map(|&i| jobs[i].0.max_parallel).unwrap_or(0) as usize;
            let active = idxs
                .iter()
                .filter(|&&i| {
                    let j = &jobs[i].0;
                    !j.held && status_of(job_of(j)) != WorkflowStatus::Completed
                })
                .count();
            let mut free = if max == 0 {
                usize::MAX
            } else {
                max.saturating_sub(active)
            };
            for &i in idxs {
                if free == 0 {
                    break;
                }
                let j = &mut jobs[i].0;
                if j.held
                    && status_of(job_of(j)) == WorkflowStatus::Queued
                    && j.concurrency_key.is_empty()
                {
                    j.held = false;
                    Box::pin(self.enqueue_job(&mut tx, repo_id, &rec, j, fx)).await?;
                    dirty.insert(i);
                    changed = true;
                    free -= 1;
                }
            }
        }

        // Graph scopes, deepest first so finished calls complete their callers.
        let mut scope_ids: Vec<String> = vec![String::new()];
        scope_ids.extend(rec.scopes.iter().filter(|s| !s.done).map(|s| s.id.clone()));
        scope_ids.sort_by_key(|s| {
            std::cmp::Reverse(s.matches('/').count() + usize::from(!s.is_empty()))
        });
        for sid in scope_ids {
            let scope = rec.scopes.iter().find(|s| s.id == sid).cloned();
            let wkey = scope
                .as_ref()
                .map(|s| s.workflow_key.clone())
                .unwrap_or_else(|| planning.top_key.clone());
            let wf = planning.workflow(&wkey)?;
            let top_wf = planning.workflow(&planning.top_key)?;
            let mut base = Object::new();
            base.insert("github", planning.github(&top_wf));
            base.insert(
                "inputs",
                match &scope {
                    Some(s) => Value::from(json_object(&s.inputs_json)),
                    None => Value::from(planning_inputs(&planning, &top_wf)),
                },
            );
            base.insert("vars", Value::from(planning.vars.clone()));
            let mut graph = RunGraph::new(&wf.jobs, base)
                .map_err(|c| ForgeError::internal(format!("cycle in stored workflow: {c:?}")))?;
            let all: Vec<&JobRecord> = jobs
                .iter()
                .map(|(j, _)| j)
                .chain(new_jobs.iter())
                .filter(|j| j.scope == sid)
                .collect();
            let outcomes = outcomes_of(&all);
            for (k, o) in &outcomes {
                graph.complete(k, o.clone());
            }
            if rec.cancelled {
                graph.cancel();
            }
            let started: BTreeSet<String> = all.iter().map(|j| j.key.to_lowercase()).collect();
            let ready: Vec<String> = graph
                .ready()
                .into_iter()
                .filter(|k| !started.contains(&k.to_lowercase()))
                .collect();
            for key in ready {
                changed = true;
                let outside_probe = sid.is_empty()
                    && !rec.only_jobs.is_empty()
                    && !rec.only_jobs.contains(&key.to_lowercase());
                let decision = if rec.force_cancelled || outside_probe {
                    Decision::Skip
                } else {
                    graph.decide(&key).unwrap_or(Decision::Skip)
                };
                let model = wf.job(&key).cloned();
                let display_prefix = scope.as_ref().map(|s| s.prefix.clone()).unwrap_or_default();
                let display = model
                    .as_ref()
                    .and_then(|m| m.name.as_ref())
                    .filter(|n| n.is_literal())
                    .map(|n| n.render())
                    .unwrap_or_else(|| key.clone());
                if decision == Decision::Skip {
                    let mut j = self.new_job(
                        &rec,
                        &sid,
                        &key,
                        &format!("{display_prefix}{display}"),
                        now,
                        &tx,
                    )?;
                    j.kind = "skip".into();
                    j.result = if rec.force_cancelled {
                        "cancelled"
                    } else {
                        "skipped"
                    }
                    .into();
                    let c = if rec.force_cancelled {
                        WorkflowConclusion::Cancelled
                    } else {
                        WorkflowConclusion::Skipped
                    };
                    {
                        let seq = next_seq(&mut tx, run_id).await?;
                        set_done(&mut j, c, now, seq);
                    }
                    new_jobs.push(j);
                    continue;
                }
                let needs: BTreeMap<String, JobOutcome> = model
                    .as_ref()
                    .map(|m| {
                        m.needs
                            .iter()
                            .filter_map(|n| {
                                let dep = wf.job(n)?;
                                outcomes
                                    .get(&dep.id.to_lowercase())
                                    .map(|o| (dep.id.clone(), o.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let planned = self.planned_job(&planning, &rec, &sid, &key, &needs);
                let planned = match planned {
                    Ok(p) => p,
                    Err(msg) => {
                        let mut j = self.new_job(
                            &rec,
                            &sid,
                            &key,
                            &format!("{display_prefix}{display}"),
                            now,
                            &tx,
                        )?;
                        j.kind = "skip".into();
                        j.result = "failure".into();
                        j.error = msg;
                        {
                            let seq = next_seq(&mut tx, run_id).await?;
                            set_done(&mut j, WorkflowConclusion::Failure, now, seq);
                        }
                        new_jobs.push(j);
                        continue;
                    }
                };
                match planned.body {
                    JobBody::Instances(legs) => {
                        let n = legs.len() as u32;
                        let mut created = Vec::new();
                        for (i, inst) in legs.iter().enumerate() {
                            let name = format!("{display_prefix}{}", inst.name);
                            let mut j = self.new_job(&rec, &sid, &key, &name, now, &tx)?;
                            let mut perms =
                                effective_permissions(inst.permissions.as_ref(), policy_write);
                            if rec.from_fork && !policy.fork_send_write_tokens {
                                for (_, access) in &mut perms {
                                    if *access == Access::Write {
                                        *access = Access::Read;
                                    }
                                }
                            }
                            j.permissions = permission_strings(&perms);
                            j.leg = i as u32;
                            j.legs = n;
                            j.kind = "step".into();
                            j.continue_on_error = inst.continue_on_error;
                            j.timeout_ms = (inst.timeout_minutes * 60_000.0).max(0.0) as u64;
                            j.fail_fast = inst.strategy.as_ref().is_none_or(|s| s.fail_fast);
                            j.max_parallel = inst
                                .strategy
                                .as_ref()
                                .map(|s| s.max_parallel as u32)
                                .unwrap_or(0);
                            j.group = inst.runs_on.group.clone().unwrap_or_default();
                            j.environment = inst
                                .environment
                                .as_ref()
                                .map(|e| e.name.clone())
                                .unwrap_or_default();
                            if let Some(c) = &inst.concurrency {
                                j.concurrency_key = bee_workflow::concurrency::group_key(&c.group);
                                j.concurrency_cancel = c.cancel_in_progress;
                                j.concurrency_queue_max = c.queue_max;
                            }
                            let mut job = job_of(&j).clone();
                            job.labels = inst.runs_on.labels.clone();
                            job.matrix_json = inst
                                .matrix
                                .as_ref()
                                .map(|m| Value::from(m.clone()).to_json_compact())
                                .unwrap_or_default();
                            job.environment = j.environment.clone();
                            job.steps = inst
                                .steps
                                .iter()
                                .enumerate()
                                .map(|(n, s)| bee_proto::v1::WorkflowStep {
                                    number: n as u32 + 1,
                                    name: s.name.render(),
                                    status: WorkflowStatus::Queued as i32,
                                    ..Default::default()
                                })
                                .collect();
                            job.runner_group_id = j.group.clone();
                            j.job = Some(job);
                            let needs_ctx = needs_json(&needs);
                            j.plan_json = plan_json(
                                inst,
                                &PlanFacts {
                                    job_id: job_of(&j).id.clone(),
                                    job_key: key.clone(),
                                    github: super::util::to_json(&planning.github(&top_wf)),
                                    inputs: super::util::to_json(&match &scope {
                                        Some(s) => Value::from(json_object(&s.inputs_json)),
                                        None => Value::from(planning_inputs(&planning, &top_wf)),
                                    }),
                                    vars: super::util::to_json(&Value::from(planning.vars.clone())),
                                    needs: needs_ctx,
                                    debug: run_of(&rec).debug_logging,
                                    permissions: j.permissions.clone(),
                                },
                            );
                            let max = j.max_parallel as usize;
                            if max > 0 && i >= max {
                                j.held = true;
                            } else {
                                Box::pin(self.enqueue_job(&mut tx, repo_id, &rec, &mut j, fx))
                                    .await?;
                            }
                            created.push(j);
                        }
                        if created.is_empty() {
                            let mut j = self.new_job(
                                &rec,
                                &sid,
                                &key,
                                &format!("{display_prefix}{display}"),
                                now,
                                &tx,
                            )?;
                            j.kind = "skip".into();
                            j.result = "skipped".into();
                            {
                                let seq = next_seq(&mut tx, run_id).await?;
                                set_done(&mut j, WorkflowConclusion::Skipped, now, seq);
                            }
                            created.push(j);
                        }
                        new_jobs.extend(created);
                    }
                    JobBody::Call(calls) => {
                        let n = calls.len() as u32;
                        for (i, call) in calls.iter().enumerate() {
                            let name = format!("{display_prefix}{}", call.name);
                            let mut j = self.new_job(&rec, &sid, &key, &name, now, &tx)?;
                            j.kind = "call".into();
                            j.leg = i as u32;
                            j.legs = n;
                            let mut job = job_of(&j).clone();
                            job.status = WorkflowStatus::InProgress as i32;
                            job.started_at = Some(ts(now));
                            job.matrix_json = call
                                .matrix
                                .as_ref()
                                .map(|m| Value::from(m.clone()).to_json_compact())
                                .unwrap_or_default();
                            j.job = Some(job);
                            match &call.workflow {
                                CalledWorkflow::Planned { .. } => {
                                    let callee = match model.as_ref().map(|m| &m.kind) {
                                        Some(JobKind::Call(c)) => workflow_key(&c.uses),
                                        _ => String::new(),
                                    };
                                    let (inherit, secrets) = match &call.secrets {
                                        CallSecretsPlan::Inherit => (true, Vec::new()),
                                        CallSecretsPlan::None => (false, Vec::new()),
                                        CallSecretsPlan::Map(m) => (
                                            false,
                                            m.iter()
                                                .map(|(k, t)| KeyValue {
                                                    key: k.clone(),
                                                    value: template_json(t).to_string(),
                                                })
                                                .collect(),
                                        ),
                                    };
                                    let new_id = format!(
                                        "{}{}{key}#{i}",
                                        sid,
                                        if sid.is_empty() { "" } else { "/" }
                                    );
                                    rec.scopes.push(ScopeRecord {
                                        id: new_id,
                                        workflow_key: callee,
                                        parent: sid.clone(),
                                        caller: key.clone(),
                                        leg: i as u32,
                                        inputs_json: object_json(&call.inputs),
                                        secrets_inherit: inherit,
                                        secrets,
                                        done: false,
                                        prefix: format!("{name} / "),
                                        caller_needs_json: needs_json(&needs).to_string(),
                                    });
                                }
                                CalledWorkflow::Unresolved(r) => {
                                    j.error =
                                        format!("reusable workflow {r} could not be resolved");
                                    j.result = "failure".into();
                                    {
                                        let seq = next_seq(&mut tx, run_id).await?;
                                        set_done(&mut j, WorkflowConclusion::Failure, now, seq);
                                    }
                                }
                            }
                            new_jobs.push(j);
                        }
                    }
                    JobBody::Skipped => {
                        let mut j = self.new_job(
                            &rec,
                            &sid,
                            &key,
                            &format!("{display_prefix}{display}"),
                            now,
                            &tx,
                        )?;
                        j.kind = "skip".into();
                        j.result = "skipped".into();
                        {
                            let seq = next_seq(&mut tx, run_id).await?;
                            set_done(&mut j, WorkflowConclusion::Skipped, now, seq);
                        }
                        new_jobs.push(j);
                    }
                    JobBody::Deferred(msg) => {
                        let mut j = self.new_job(
                            &rec,
                            &sid,
                            &key,
                            &format!("{display_prefix}{display}"),
                            now,
                            &tx,
                        )?;
                        j.kind = "skip".into();
                        j.result = "failure".into();
                        j.error = format!("job could not be planned: {msg}");
                        {
                            let seq = next_seq(&mut tx, run_id).await?;
                            set_done(&mut j, WorkflowConclusion::Failure, now, seq);
                        }
                        new_jobs.push(j);
                    }
                }
            }
            // A finished nested scope completes its caller leg.
            if let Some(s) = &scope {
                let all: Vec<&JobRecord> = jobs
                    .iter()
                    .map(|(j, _)| j)
                    .chain(new_jobs.iter())
                    .filter(|j| j.scope == sid)
                    .collect();
                let outs = outcomes_of(&all);
                let finished = wf
                    .jobs
                    .iter()
                    .all(|j| outs.contains_key(&j.id.to_lowercase()));
                if finished {
                    let result = scope_result(&outs, rec.cancelled);
                    let outputs = call_outputs(&wf, &outs);
                    let caller_idx = jobs.iter().position(|(j, _)| {
                        j.scope == s.parent
                            && j.key.eq_ignore_ascii_case(&s.caller)
                            && j.leg == s.leg
                            && j.kind == "call"
                    });
                    if let Some(i) = caller_idx {
                        let j = &mut jobs[i].0;
                        j.outputs = outputs;
                        j.result = result_str(result).into();
                        {
                            let seq = next_seq(&mut tx, run_id).await?;
                            set_done(j, result, now, seq);
                        }
                        dirty.insert(i);
                    } else if let Some(j) = new_jobs.iter_mut().find(|j| {
                        j.scope == s.parent
                            && j.key.eq_ignore_ascii_case(&s.caller)
                            && j.leg == s.leg
                            && j.kind == "call"
                    }) {
                        j.outputs = outputs;
                        j.result = result_str(result).into();
                        {
                            let seq = next_seq(&mut tx, run_id).await?;
                            set_done(j, result, now, seq);
                        }
                    }
                    if let Some(sc) = rec.scopes.iter_mut().find(|x| x.id == sid) {
                        sc.done = true;
                    }
                    changed = true;
                }
            }
        }

        // The run finishes with its top-level graph.
        let top_wf = planning.workflow(&planning.top_key)?;
        let top: Vec<&JobRecord> = jobs
            .iter()
            .map(|(j, _)| j)
            .chain(new_jobs.iter())
            .filter(|j| j.scope.is_empty())
            .collect();
        let outs = outcomes_of(&top);
        let all_open = jobs
            .iter()
            .map(|(j, _)| j)
            .chain(new_jobs.iter())
            .any(|j| status_of(job_of(j)) != WorkflowStatus::Completed);
        let mut completed = false;
        if top_wf
            .jobs
            .iter()
            .all(|j| outs.contains_key(&j.id.to_lowercase()))
            && !all_open
        {
            let result = scope_result(&outs, rec.cancelled);
            let mut run = run_of(&rec).clone();
            run.status = WorkflowStatus::Completed as i32;
            run.conclusion = result as i32;
            run.completed_at = Some(ts(now));
            run.updated_at = Some(ts(now));
            rec.run = Some(run);
            if !rec.concurrency_key.is_empty() {
                let key = rec.concurrency_key.clone();
                self.conc_leave(&mut tx, repo_id, &key, run_id, fx).await?;
            }
            self.emit_run(&mut tx, &rec, "workflow_run.completed")
                .await?;
            tx.audit(
                AuditDraft::new("workflow.run_completed", &rec.org_id)
                    .repo_id(repo_id)
                    .target(SubjectKind::WorkflowRun, run_id)
                    .after(&serde_json::json!({ "conclusion": result.as_str_name() }).to_string()),
            )
            .await?;
            completed = true;
            changed = true;
        }
        if !changed && dirty.is_empty() && new_jobs.is_empty() {
            return Ok(false);
        }
        for i in dirty {
            let (j, row) = &jobs[i];
            tx.update(&JOBS, &job_row(repo_id, j).version(row.version))
                .await?;
        }
        for j in &new_jobs {
            tx.insert(&JOBS, &job_row(repo_id, j)).await?;
            if j.kind == "step" && status_of(job_of(j)) != WorkflowStatus::Completed {
                Self::emit_job(&mut tx, repo_id, j, "workflow_job.queued").await?;
            }
        }
        let mut run = run_of(&rec).clone();
        run.updated_at = Some(ts(now));
        rec.run = Some(run);
        tx.update(&RUNS, &run_row(repo_id, &rec).version(run_row_v.version))
            .await?;
        tx.commit().await?;
        fx.sync.insert((repo_id.to_string(), run_id.to_string()));
        if completed {
            fx.completed.push((repo_id.to_string(), run_id.to_string()));
            return Ok(false);
        }
        Ok(true)
    }

    fn new_job(
        &self,
        rec: &RunRecord,
        scope: &str,
        key: &str,
        name: &str,
        now: u64,
        tx: &Tx,
    ) -> Result<JobRecord> {
        let run = run_of(rec);
        Ok(JobRecord {
            job: Some(WorkflowJob {
                id: tx.new_id()?,
                run_id: run.id.clone(),
                run_attempt: run.run_attempt,
                name: name.to_string(),
                workflow_name: run.name.clone(),
                status: WorkflowStatus::Queued as i32,
                head_ref: run.head_ref.clone(),
                head_changeset: run.head_changeset.clone(),
                created_at: Some(ts(now)),
                ..Default::default()
            }),
            org_id: rec.org_id.clone(),
            scope: scope.to_string(),
            key: key.to_string(),
            legs: 1,
            check_synced: -1,
            ..Default::default()
        })
    }

    /// The planned job `key` of a scope.
    fn planned_job(
        &self,
        p: &Planning,
        rec: &RunRecord,
        scope_id: &str,
        key: &str,
        needs: &BTreeMap<String, JobOutcome>,
    ) -> std::result::Result<PlannedJob, String> {
        let resolver = MapResolver(&p.files);
        let mut input = p.input();
        input.resolver = Some(&resolver);
        if scope_id.is_empty() {
            let wf = p.workflow(&p.top_key).map_err(|e| e.message)?;
            return plan_job(&wf, key, p.schedule.as_deref(), &input, needs)
                .map_err(|e| e.to_string());
        }
        let scope = rec
            .scopes
            .iter()
            .find(|s| s.id == scope_id)
            .ok_or_else(|| format!("unknown scope {scope_id}"))?;
        let caller_needs = needs_from_json(&scope.caller_needs_json);
        let caller = self.planned_job(p, rec, &scope.parent, &scope.caller, &caller_needs)?;
        let JobBody::Call(calls) = caller.body else {
            return Err(format!("{} is not a reusable workflow call", scope.caller));
        };
        let call = calls
            .get(scope.leg as usize)
            .ok_or_else(|| format!("call leg {} missing", scope.leg))?;
        let CalledWorkflow::Planned { jobs, .. } = &call.workflow else {
            return Err("the called workflow could not be resolved".into());
        };
        let job = jobs
            .iter()
            .find(|j| j.id.eq_ignore_ascii_case(key))
            .cloned()
            .ok_or_else(|| format!("job {key} missing from the called workflow"))?;
        match &job.body {
            JobBody::Deferred(m) => Err(format!(
                "a job in a called workflow whose pre-run settings read `needs` outputs is not supported: {m}"
            )),
            _ => Ok(job),
        }
    }

    // ----- queueing ---------------------------------------------------------------------------

    /// Puts a job on its way to a runner: job concurrency, environment protection, then the
    /// queue.
    pub(crate) async fn enqueue_job(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        rec: &RunRecord,
        j: &mut JobRecord,
        fx: &mut Effects,
    ) -> Result<()> {
        let now = tx.now_ms();
        let job_id = job_of(j).id.clone();
        if !j.concurrency_key.is_empty() && !j.concurrency_released {
            let start = self
                .conc_join(
                    tx,
                    repo_id,
                    &j.concurrency_key.clone(),
                    &format!("job:{job_id}"),
                    &job_of(j).run_id.clone(),
                    j.concurrency_cancel,
                    j.concurrency_queue_max,
                    fx,
                )
                .await?;
            if !start {
                j.held = true;
                set_status(j, WorkflowStatus::Pending, now);
                return Ok(());
            }
            j.concurrency_released = true;
        }
        j.held = false;
        if !j.environment.is_empty() && j.env_state != "approved" {
            let git = git_ref(&run_of(rec).head_ref);
            match Box::pin(self.gate(tx, repo_id, rec, j, &git, fx)).await? {
                super::environments::Gate::Proceed => {}
                super::environments::Gate::Wait => {
                    let was_waiting = status_of(job_of(j)) == WorkflowStatus::Waiting;
                    set_status(j, WorkflowStatus::Waiting, now);
                    if !was_waiting {
                        Self::emit_job(tx, repo_id, j, "workflow_job.waiting").await?;
                    }
                    if j.wait_until_ms > 0 {
                        tx.put(
                            &JOB_INDEX,
                            &Row::new("", job_id.clone()).n1(j.wait_until_ms as i64).msg(
                                &JobIndex {
                                    repo_id: repo_id.into(),
                                    org_id: j.org_id.clone(),
                                    run_id: job_of(j).run_id.clone(),
                                },
                            ),
                        )
                        .await?;
                    }
                    return Ok(());
                }
                super::environments::Gate::Reject(msg) => {
                    j.error = msg;
                    self.finish_job(tx, repo_id, j, WorkflowConclusion::Failure, "", fx)
                        .await?;
                    return Ok(());
                }
            }
        }
        set_status(j, WorkflowStatus::Queued, now);
        let job = job_of(j);
        let repo_public = rec.repo_public;
        tx.put(
            &QUEUE,
            &Row::new("", job_id.clone())
                .k1(repo_id)
                .n1(now as i64)
                .msg(&QueueEntry {
                    repo_id: repo_id.into(),
                    org_id: j.org_id.clone(),
                    labels: job.labels.clone(),
                    group: j.group.clone(),
                    public: repo_public,
                    workflow_ref: run_of(rec).workflow_path.clone(),
                }),
        )
        .await?;
        tx.put(
            &JOB_INDEX,
            &Row::new("", job_id.clone()).msg(&JobIndex {
                repo_id: repo_id.into(),
                org_id: j.org_id.clone(),
                run_id: job.run_id.clone(),
            }),
        )
        .await?;
        fx.wake = true;
        Ok(())
    }

    /// Completes a job inside `tx`: removes it from the queue and index, frees its runner and
    /// concurrency group, meters usage, and records the deployment outcome.
    pub(crate) async fn finish_job(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        j: &mut JobRecord,
        conclusion: WorkflowConclusion,
        _reason: &str,
        fx: &mut Effects,
    ) -> Result<()> {
        if status_of(job_of(j)) == WorkflowStatus::Completed {
            return Ok(());
        }
        let now = tx.now_ms();
        let job_id = job_of(j).id.clone();
        let run_id = job_of(j).run_id.clone();
        let seq = next_seq(tx, &run_id).await?;
        let mut job = job_of(j).clone();
        let started = job.started_at.as_ref().map(ts_ms);
        job.status = WorkflowStatus::Completed as i32;
        job.conclusion = conclusion as i32;
        job.completed_at = Some(ts(now));
        for s in &mut job.steps {
            if s.status != WorkflowStatus::Completed as i32 {
                s.status = WorkflowStatus::Completed as i32;
                s.conclusion = if conclusion == WorkflowConclusion::Success {
                    WorkflowConclusion::Skipped as i32
                } else {
                    conclusion as i32
                };
            }
        }
        j.job = Some(job.clone());
        if j.result.is_empty() {
            j.result = result_str(conclusion).into();
        }
        j.completed_seq = seq;
        j.held = false;
        tx.delete(&QUEUE, "", &job_id).await?;
        tx.delete(&JOB_INDEX, "", &job_id).await?;
        if !j.runner_id.is_empty()
            && let Some((mut r, row)) = tx
                .get_msg::<RunnerRecord>(&RUNNERS, "", &j.runner_id)
                .await?
        {
            r.jobs.retain(|x| *x != job_id);
            let mut runner = r.runner.clone().unwrap_or_default();
            runner.busy = !r.jobs.is_empty();
            runner.current_job_id = r.jobs.first().cloned().unwrap_or_default();
            r.runner = Some(runner.clone());
            if runner.ephemeral && r.jobs.is_empty() {
                tx.delete(&RUNNERS, "", &j.runner_id).await?;
            } else {
                tx.update(
                    &RUNNERS,
                    &super::runners::runner_row(&r).version(row.version),
                )
                .await?;
            }
        }
        if !j.concurrency_key.is_empty() {
            let key = j.concurrency_key.clone();
            self.conc_leave(tx, repo_id, &key, &format!("job:{job_id}"), fx)
                .await?;
        }
        if let Some(start) = started
            && j.kind == "step"
        {
            let duration = now.saturating_sub(start);
            let class = if job.machine_class.is_empty() {
                "self-hosted".to_string()
            } else {
                job.machine_class.clone()
            };
            let billable = if job.machine_class.is_empty() {
                0
            } else {
                duration.div_ceil(60_000) * 60_000
            };
            let workflow_id = tx
                .get_msg::<RunRecord>(&RUNS, repo_id, &run_id)
                .await?
                .map(|(r, _)| run_of(&r).workflow_id.clone())
                .unwrap_or_default();
            let usage = UsageRecord {
                job_id: job_id.clone(),
                run_id: run_id.clone(),
                workflow_id: workflow_id.clone(),
                machine_class: class,
                duration_ms: duration,
                billable_ms: billable,
                completed_ms: now,
            };
            tx.put(
                &USAGE,
                &Row::new(repo_id, job_id.clone())
                    .k1(workflow_id)
                    .n1(now as i64)
                    .msg(&usage),
            )
            .await?;
            tx.emit_event(Event {
                kind: "workflow_usage.recorded".into(),
                repo_id: repo_id.into(),
                org_id: j.org_id.clone(),
                subject: Some(SubjectRef {
                    kind: SubjectKind::WorkflowRun as i32,
                    repo: repo_ref_of(repo_id),
                    id: run_id.clone(),
                }),
                payload_json: serde_json::json!({
                    "job_id": usage.job_id, "run_id": usage.run_id,
                    "workflow_id": usage.workflow_id, "machine_class": usage.machine_class,
                    "duration_ms": usage.duration_ms, "billable_ms": usage.billable_ms,
                })
                .to_string(),
                ..Default::default()
            })
            .await?;
        }
        if !j.deployment_id.is_empty() {
            let state = match conclusion {
                WorkflowConclusion::Success => bee_proto::v1::DeploymentState::Success,
                WorkflowConclusion::Cancelled => bee_proto::v1::DeploymentState::Inactive,
                _ => bee_proto::v1::DeploymentState::Failure,
            };
            self.deployment_progress_url(
                tx,
                repo_id,
                &j.deployment_id.clone(),
                state,
                &j.org_id.clone(),
                &j.environment_url.clone(),
            )
            .await?;
        }
        Self::emit_job(tx, repo_id, j, "workflow_job.completed").await?;
        fx.advance.insert((repo_id.to_string(), run_id));
        fx.wake = true;
        Ok(())
    }

    // ----- concurrency ------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn conc_join(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        key: &str,
        member: &str,
        run_id: &str,
        cancel_in_progress: bool,
        queue_max: bool,
        fx: &mut Effects,
    ) -> Result<bool> {
        let (mut group, version) = match tx
            .get_msg::<ConcurrencyGroup>(&CONCURRENCY, repo_id, key)
            .await?
        {
            Some((g, r)) => (g, r.version),
            None => (ConcurrencyGroup::default(), 0),
        };
        group.members.retain(|m| m.id != member);
        let members: Vec<Member> = group
            .members
            .iter()
            .map(|m| Member {
                id: m.id.clone(),
                state: if m.running {
                    MemberState::Running
                } else {
                    MemberState::Pending
                },
            })
            .collect();
        let decision = admit(
            &members,
            ConcRequest {
                cancel_in_progress,
                queue_max,
            },
        );
        for c in &decision.cancel {
            let Some(m) = group.members.iter().find(|m| m.id == *c).cloned() else {
                continue;
            };
            group.members.retain(|x| x.id != *c);
            if let Some(job_id) = c.strip_prefix("job:") {
                if let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, repo_id, job_id).await? {
                    j.concurrency_key.clear();
                    if status_of(job_of(&j)) == WorkflowStatus::InProgress {
                        j.cancel_requested = true;
                    } else {
                        self.finish_job(tx, repo_id, &mut j, WorkflowConclusion::Cancelled, "", fx)
                            .await?;
                    }
                    tx.update(&JOBS, &job_row(repo_id, &j).version(row.version))
                        .await?;
                }
            } else {
                fx.cancel_runs
                    .insert((repo_id.to_string(), m.run_id.clone()));
            }
        }
        group.members.push(ConcurrencyMember {
            id: member.to_string(),
            running: decision.start,
            run_id: run_id.to_string(),
        });
        let row = Row::new(repo_id, key).version(version).msg(&group);
        if version == 0 {
            tx.insert(&CONCURRENCY, &row).await?;
        } else {
            tx.update(&CONCURRENCY, &row).await?;
        }
        Ok(decision.start)
    }

    async fn conc_leave(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        key: &str,
        member: &str,
        fx: &mut Effects,
    ) -> Result<()> {
        let Some((mut group, row)) = tx
            .get_msg::<ConcurrencyGroup>(&CONCURRENCY, repo_id, key)
            .await?
        else {
            return Ok(());
        };
        let before = group.members.len();
        group.members.retain(|m| m.id != member);
        if group.members.len() == before {
            return Ok(());
        }
        let members: Vec<Member> = group
            .members
            .iter()
            .map(|m| Member {
                id: m.id.clone(),
                state: if m.running {
                    MemberState::Running
                } else {
                    MemberState::Pending
                },
            })
            .collect();
        let next = bee_workflow::concurrency::next_to_start(&members).map(str::to_string);
        if let Some(next) = &next
            && let Some(m) = group.members.iter_mut().find(|m| m.id == *next)
        {
            m.running = true;
        }
        if group.members.is_empty() {
            tx.delete(&CONCURRENCY, repo_id, key).await?;
        } else {
            tx.update(
                &CONCURRENCY,
                &Row::new(repo_id, key).version(row.version).msg(&group),
            )
            .await?;
        }
        let Some(next) = next else { return Ok(()) };
        if let Some(job_id) = next.strip_prefix("job:") {
            if let Some((mut j, jrow)) = tx.get_msg::<JobRecord>(&JOBS, repo_id, job_id).await? {
                let run_id = job_of(&j).run_id.clone();
                if let Some((rec, _)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, &run_id).await? {
                    j.concurrency_released = true;
                    Box::pin(self.enqueue_job(tx, repo_id, &rec, &mut j, fx)).await?;
                    tx.update(&JOBS, &job_row(repo_id, &j).version(jrow.version))
                        .await?;
                }
            }
        } else if let Some((mut rec, rrow)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, &next).await?
        {
            rec.concurrency_released = true;
            tx.update(&RUNS, &run_row(repo_id, &rec).version(rrow.version))
                .await?;
            fx.start_runs.insert((repo_id.to_string(), next.clone()));
        }
        Ok(())
    }

    // ----- cancellation, approval, re-runs ----------------------------------------------------

    /// Cancels a run. Queued jobs are cancelled at once; running jobs are asked to stop (their
    /// runner learns it on the next heartbeat) and `always()` jobs still run. `force` also
    /// completes running jobs immediately and skips every job not yet started.
    pub async fn cancel_run(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        force: bool,
    ) -> Result<()> {
        let fx = self.cancel_inner(cx, repo_id, run_id, force).await?;
        self.apply(cx, fx).await;
        Ok(())
    }

    async fn cancel_inner(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        force: bool,
    ) -> Result<Effects> {
        let _g = self.lock(run_id).lock_owned().await;
        let mut fx = Effects::default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, run_id).await? else {
            return Err(ForgeError::not_found(format!("workflow run {run_id}")));
        };
        let st = run_status(run_of(&rec));
        if st == WorkflowStatus::Completed {
            return Err(ForgeError::invalid_state("the run already completed"));
        }
        let now = tx.now_ms();
        rec.cancelled = true;
        rec.force_cancelled |= force;
        let attempt = run_of(&rec).run_attempt;
        for (mut j, jrow) in Self::run_jobs(&mut tx, repo_id, run_id, Some(attempt)).await? {
            let s = status_of(job_of(&j));
            if s == WorkflowStatus::Completed {
                continue;
            }
            if s == WorkflowStatus::InProgress && j.kind == "step" && !force {
                j.cancel_requested = true;
            } else if j.kind == "call" && !force {
                continue;
            } else {
                j.cancel_requested = true;
                j.force_cancel = force;
                j.result = "cancelled".into();
                self.finish_job(
                    &mut tx,
                    repo_id,
                    &mut j,
                    WorkflowConclusion::Cancelled,
                    "",
                    &mut fx,
                )
                .await?;
            }
            tx.update(&JOBS, &job_row(repo_id, &j).version(jrow.version))
                .await?;
        }
        // Re-read: finishing jobs advanced the completion counter.
        let (mut fresh, fresh_row) = tx
            .get_msg::<RunRecord>(&RUNS, repo_id, run_id)
            .await?
            .unwrap_or((rec.clone(), row.clone()));
        fresh.cancelled = true;
        fresh.force_cancelled = rec.force_cancelled;
        let mut run = run_of(&fresh).clone();
        if matches!(
            st,
            WorkflowStatus::Requested | WorkflowStatus::Pending | WorkflowStatus::ActionRequired
        ) {
            run.status = WorkflowStatus::Completed as i32;
            run.conclusion = WorkflowConclusion::Cancelled as i32;
            run.completed_at = Some(ts(now));
            if !fresh.concurrency_key.is_empty() {
                let key = fresh.concurrency_key.clone();
                self.conc_leave(&mut tx, repo_id, &key, run_id, &mut fx)
                    .await?;
            }
        }
        run.updated_at = Some(ts(now));
        fresh.run = Some(run);
        tx.update(&RUNS, &run_row(repo_id, &fresh).version(fresh_row.version))
            .await?;
        tx.audit(
            AuditDraft::new(
                if force {
                    "workflow.run_force_cancel"
                } else {
                    "workflow.run_cancel"
                },
                &fresh.org_id,
            )
            .repo_id(repo_id)
            .target(SubjectKind::WorkflowRun, run_id),
        )
        .await?;
        if run_status(run_of(&fresh)) == WorkflowStatus::Completed {
            self.emit_run(&mut tx, &fresh, "workflow_run.completed")
                .await?;
        }
        tx.commit().await?;
        fx.advance.insert((repo_id.to_string(), run_id.to_string()));
        fx.wake = true;
        Ok(fx)
    }

    /// Approves a run held for approval and starts it.
    pub async fn approve_run(&self, cx: &Cx, repo_id: &str, run_id: &str) -> Result<WorkflowRun> {
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((mut rec, row)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, run_id).await?
            else {
                return Err(ForgeError::not_found(format!("workflow run {run_id}")));
            };
            if run_status(run_of(&rec)) != WorkflowStatus::ActionRequired {
                return Err(ForgeError::invalid_state(
                    "the run is not awaiting approval",
                ));
            }
            let now = tx.now_ms();
            let mut run = run_of(&rec).clone();
            run.approval_required = false;
            run.status = WorkflowStatus::Requested as i32;
            run.updated_at = Some(ts(now));
            rec.run = Some(run);
            tx.update(&RUNS, &run_row(repo_id, &rec).version(row.version))
                .await?;
            let approval = bee_proto::v1::WorkflowRunApproval {
                kind: bee_proto::v1::WorkflowApprovalKind::Run as i32,
                approver: Some(cx.principal()),
                decision: bee_proto::v1::DeploymentReviewDecision::Approved as i32,
                created_at: Some(ts(now)),
                ..Default::default()
            };
            let id = tx.new_id()?;
            tx.insert(
                &super::APPROVALS,
                &Row::new(run_id, id).msg(&super::model::ApprovalRecord {
                    approval: Some(approval),
                }),
            )
            .await?;
            tx.audit(
                AuditDraft::new("workflow.run_approve", &rec.org_id)
                    .repo_id(repo_id)
                    .target(SubjectKind::WorkflowRun, run_id),
            )
            .await?;
            tx.commit().await?;
        }
        self.start_run(cx, repo_id, run_id).await?;
        self.get_run(cx, repo_id, run_id).await
    }

    /// Re-runs a completed run as a new attempt: every job (`Rerun::All`), failed and
    /// cancelled jobs and their dependents (`Rerun::Failed`), or one job and its dependents
    /// (`Rerun::Job`). Jobs not re-run are carried into the new attempt with their results.
    pub async fn rerun(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        what: Rerun,
        debug: bool,
    ) -> Result<WorkflowRun> {
        {
            let _g = self.lock(run_id).lock_owned().await;
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((mut rec, row)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, run_id).await?
            else {
                return Err(ForgeError::not_found(format!("workflow run {run_id}")));
            };
            let run = run_of(&rec).clone();
            if run_status(&run) != WorkflowStatus::Completed {
                return Err(ForgeError::invalid_state("the run has not completed"));
            }
            if !rec.startup_error.is_empty() {
                return Err(ForgeError::invalid_state(
                    "the run failed to start; fix the workflow and push again",
                ));
            }
            let now = tx.now_ms();
            let window = u64::from(self.config.rerun_window_days) * 86_400_000;
            if run.completed_at.as_ref().map(ts_ms).unwrap_or(now) + window < now {
                return Err(ForgeError::invalid_state(
                    "the run is older than the re-run window",
                ));
            }
            let jobs = Self::run_jobs(&mut tx, repo_id, run_id, Some(run.run_attempt)).await?;
            let planning = Planning::of(&rec);
            let top = planning.workflow(&planning.top_key)?;
            // Top-level keys to re-run.
            let mut rerun: BTreeSet<String> = BTreeSet::new();
            let top_key_of = |j: &JobRecord| -> String {
                if j.scope.is_empty() {
                    j.key.to_lowercase()
                } else {
                    j.scope
                        .split('/')
                        .next()
                        .and_then(|s| s.split('#').next())
                        .unwrap_or("")
                        .to_lowercase()
                }
            };
            match &what {
                Rerun::All => {
                    rerun.extend(top.jobs.iter().map(|j| j.id.to_lowercase()));
                }
                Rerun::Failed => {
                    for (j, _) in &jobs {
                        if matches!(j.result.as_str(), "failure" | "cancelled" | "timed_out") {
                            rerun.insert(top_key_of(j));
                        }
                    }
                    if rerun.is_empty() {
                        return Err(ForgeError::invalid_state("the run has no failed jobs"));
                    }
                }
                Rerun::Job(id) => {
                    let Some((j, _)) = jobs.iter().find(|(j, _)| job_of(j).id == *id) else {
                        return Err(ForgeError::not_found(format!(
                            "job {id} in the latest attempt"
                        )));
                    };
                    rerun.insert(top_key_of(j));
                }
            }
            // Dependents, transitively.
            loop {
                let before = rerun.len();
                for j in &top.jobs {
                    if j.needs.iter().any(|n| rerun.contains(&n.to_lowercase())) {
                        rerun.insert(j.id.to_lowercase());
                    }
                }
                if rerun.len() == before {
                    break;
                }
            }
            let new_attempt = run.run_attempt + 1;
            let attempt_id = format!("{:010}", run.run_attempt);
            tx.put(&ATTEMPTS, &Row::new(run_id, attempt_id).msg(&run))
                .await?;
            for (j, _) in &jobs {
                if rerun.contains(&top_key_of(j)) {
                    continue;
                }
                let mut c = j.clone();
                let mut job = job_of(j).clone();
                job.id = tx.new_id()?;
                job.run_attempt = new_attempt;
                c.job = Some(job);
                c.carried = true;
                tx.insert(&JOBS, &job_row(repo_id, &c)).await?;
            }
            rec.scopes
                .retain(|s| !rerun.contains(&s.id.split('#').next().unwrap_or("").to_lowercase()));
            let mut next = run.clone();
            next.run_attempt = new_attempt;
            next.status = WorkflowStatus::Requested as i32;
            next.conclusion = WorkflowConclusion::Unspecified as i32;
            next.previous_attempt_id = format!("{run_id}/{}", run.run_attempt);
            next.triggering_actor = Some(cx.principal());
            next.debug_logging = debug;
            next.completed_at = None;
            next.run_started_at = None;
            next.updated_at = Some(ts(now));
            next.check_suite_id = String::new();
            rec.run = Some(next);
            rec.cancelled = false;
            rec.force_cancelled = false;
            rec.concurrency_released = false;
            tx.update(&RUNS, &run_row(repo_id, &rec).version(row.version))
                .await?;
            self.emit_run(&mut tx, &rec, "workflow_run.requested")
                .await?;
            tx.audit(
                AuditDraft::new("workflow.run_rerun", &rec.org_id)
                    .repo_id(repo_id)
                    .target(SubjectKind::WorkflowRun, run_id)
                    .after(
                        &serde_json::json!({
                            "attempt": new_attempt,
                            "jobs": rerun.iter().collect::<Vec<_>>(),
                            "debug": debug,
                        })
                        .to_string(),
                    ),
            )
            .await?;
            tx.commit().await?;
        }
        self.start_run(cx, repo_id, run_id).await?;
        self.get_run(cx, repo_id, run_id).await
    }

    /// The latest attempt of a run.
    pub async fn get_run(&self, cx: &Cx, repo_id: &str, run_id: &str) -> Result<WorkflowRun> {
        Ok(served_run(&self.run_record(cx, repo_id, run_id).await?))
    }

    /// A run's record.
    pub(crate) async fn run_record(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
    ) -> Result<RunRecord> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<RunRecord>(&RUNS, repo_id, run_id)
            .await?
            .map(|(r, _)| r)
            .ok_or_else(|| ForgeError::not_found(format!("workflow run {run_id}")))
    }

    /// The jobs of a run (`attempt` 0 = latest; `all` = every attempt).
    pub async fn list_jobs(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
        attempt: u32,
        all: bool,
    ) -> Result<Vec<WorkflowJob>> {
        let rec = self.run_record(cx, repo_id, run_id).await?;
        let a = if all {
            None
        } else if attempt == 0 {
            Some(run_of(&rec).run_attempt)
        } else {
            Some(attempt)
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut v: Vec<WorkflowJob> = Self::run_jobs(&mut tx, repo_id, run_id, a)
            .await?
            .into_iter()
            .map(|(j, _)| job_of(&j).clone())
            .collect();
        v.sort_by(|a, b| (a.run_attempt, &a.id).cmp(&(b.run_attempt, &b.id)));
        Ok(v)
    }

    /// One job's record.
    pub(crate) async fn job_record(
        &self,
        cx: &Cx,
        repo_id: &str,
        job_id: &str,
    ) -> Result<JobRecord> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<JobRecord>(&JOBS, repo_id, job_id)
            .await?
            .map(|(r, _)| r)
            .ok_or_else(|| ForgeError::not_found(format!("workflow job {job_id}")))
    }

    // ----- check runs -------------------------------------------------------------------------

    /// Reports each job of a run as a check run (and its run as a check suite), bringing every
    /// check run up to date with its job. Idempotent; the tick retries what failed.
    pub async fn sync_checks(&self, cx: &Cx, repo_id: &str, run_id: &str) -> Result<()> {
        let rec = self.run_record(cx, repo_id, run_id).await?;
        let run = run_of(&rec).clone();
        let Some(cs) = object_id(&rec.check_changeset) else {
            return Ok(());
        };
        if !rec.startup_error.is_empty() {
            return Ok(());
        }
        let post_land = rec.event.as_ref().is_some_and(|e| {
            e.name == "push"
                && rec
                    .repo_info
                    .as_ref()
                    .is_some_and(|i| e.git_ref == git_ref(&i.default_branch))
        });
        let mut suite_id = run.check_suite_id.clone();
        if suite_id.is_empty() {
            let suite = self
                .checks
                .create_suite(cx, &rec.org_id, repo_id, Some(cs.clone()), &run.head_ref)
                .await?;
            suite_id = suite.id.clone();
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if let Some((mut r, row)) = tx.get_msg::<RunRecord>(&RUNS, repo_id, run_id).await? {
                let mut x = run_of(&r).clone();
                x.check_suite_id = suite_id.clone();
                r.run = Some(x);
                tx.update(&RUNS, &run_row(repo_id, &r).version(row.version))
                    .await?;
                tx.commit().await?;
            }
        }
        let jobs = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::run_jobs(&mut tx, repo_id, run_id, Some(run.run_attempt)).await?
        };
        for (j, _) in jobs {
            if j.kind == "call" {
                continue;
            }
            let job = job_of(&j).clone();
            let st = status_of(&job);
            let want = match st {
                WorkflowStatus::InProgress => CheckStatus::Running,
                WorkflowStatus::Completed => check_status(
                    WorkflowConclusion::try_from(job.conclusion)
                        .unwrap_or(WorkflowConclusion::Failure),
                ),
                _ => CheckStatus::Queued,
            };
            if j.check_synced == want as i32 && !job.check_run_id.is_empty() {
                continue;
            }
            let output = if st == WorkflowStatus::Completed {
                let log = self.job_log_text(cx, &j).await.unwrap_or_default();
                let mut annotations = super::util::annotations(&log, 1000);
                if !j.error.is_empty() {
                    annotations.push(bee_proto::v1::CheckAnnotation {
                        path: b".github".to_vec(),
                        level: bee_proto::v1::AnnotationLevel::Failure as i32,
                        message: j.error.clone(),
                        title: job.name.clone(),
                        ..Default::default()
                    });
                }
                Some(CheckOutput {
                    title: format!(
                        "{} {}",
                        job.name,
                        result_str(
                            WorkflowConclusion::try_from(job.conclusion).unwrap_or_default()
                        )
                    ),
                    summary: if j.summary.is_empty() {
                        j.error.clone()
                    } else {
                        j.summary.clone()
                    },
                    text: String::new(),
                    annotations,
                    images: Vec::new(),
                })
            } else {
                None
            };
            let url = format!("{}/job/{}", run.html_url, job.id);
            let check_id = if job.check_run_id.is_empty() {
                let r = self
                    .checks
                    .report(
                        cx,
                        &rec.org_id,
                        repo_id,
                        Report {
                            changeset: Some(cs.clone()),
                            name: job.name.clone(),
                            status: want,
                            url,
                            summary: output
                                .as_ref()
                                .map(|o| o.summary.clone())
                                .unwrap_or_default(),
                            post_land,
                            external_id: format!("workflow_job:{}", job.id),
                            output,
                            suite_id: suite_id.clone(),
                            actions: Vec::new(),
                            started_at_ms: job.started_at.as_ref().map(ts_ms).unwrap_or(0),
                            finished_at_ms: job.completed_at.as_ref().map(ts_ms).unwrap_or(0),
                            workflow_job_id: job.id.clone(),
                        },
                    )
                    .await?;
                r.id
            } else {
                self.checks
                    .update(
                        cx,
                        &rec.org_id,
                        repo_id,
                        &job.check_run_id,
                        RunPatch {
                            status: Some(want),
                            url: None,
                            summary: output.as_ref().map(|o| o.summary.clone()),
                            output,
                            actions: Vec::new(),
                            finished_at_ms: job.completed_at.as_ref().map(ts_ms).unwrap_or(0),
                        },
                    )
                    .await?;
                job.check_run_id.clone()
            };
            let mut tx = self.db.begin(SURFACE, cx).await?;
            if let Some((mut fresh, row)) = tx.get_msg::<JobRecord>(&JOBS, repo_id, &job.id).await?
            {
                let mut fj = job_of(&fresh).clone();
                fj.check_run_id = check_id;
                fresh.job = Some(fj);
                if status_of(job_of(&fresh)) == st {
                    fresh.check_synced = want as i32;
                }
                tx.update(&JOBS, &job_row(repo_id, &fresh).version(row.version))
                    .await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }

    // ----- workflow_run chaining --------------------------------------------------------------

    async fn chain_workflow_run(&self, cx: &Cx, repo_id: &str, run_id: &str) -> Result<()> {
        let rec = self.run_record(cx, repo_id, run_id).await?;
        if rec.probe {
            return Ok(());
        }
        let run = run_of(&rec);
        let repo = self.repo_by_id(cx, repo_id).await?;
        let ev = BeeEvent::WorkflowRun(WorkflowRunEvent {
            action: "completed".into(),
            workflow_name: run.name.clone(),
            conclusion: Some(
                result_str(WorkflowConclusion::try_from(run.conclusion).unwrap_or_default())
                    .to_string(),
            ),
            head_branch: run.head_ref.clone(),
            head_sha: run
                .head_changeset
                .as_ref()
                .map(|c| hex(&c.raw))
                .unwrap_or_default(),
            event: run.event.clone(),
            run_id: rec.numeric_id,
            chain_depth: rec.chain_depth,
        });
        let cause = Cause {
            actor: run
                .triggering_actor
                .as_ref()
                .map(|p| p.id.clone())
                .unwrap_or_default(),
            by_workflow_token: rec.by_workflow_token,
        };
        let ctx = TriggerContext {
            chain_depth: rec.chain_depth + 1,
            actor: run.triggering_actor.clone(),
            ..Default::default()
        };
        self.trigger(cx, &repo, &ev, &cause, &ctx).await?;
        Ok(())
    }

    // ----- maintenance ------------------------------------------------------------------------

    /// One maintenance pass: lost leases (requeue or fail), job timeouts, elapsed wait timers,
    /// artifact and cache retention, expired registration tokens, and check-run reconciliation
    /// of unfinished runs.
    pub async fn tick(&self, cx: &Cx) -> Result<()> {
        let now = self.now();
        let due = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &JOB_INDEX,
                &Scan {
                    n1_max: Some(now as i64),
                    order: crate::store::Order::N1,
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?
        };
        for row in due {
            let idx: JobIndex = row.decode()?;
            let mut fx = Effects::default();
            let guard = self.lock(&idx.run_id).lock_owned().await;
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((mut j, jrow)) = tx
                .get_msg::<JobRecord>(&JOBS, &idx.repo_id, &row.id)
                .await?
            else {
                tx.delete(&JOB_INDEX, "", &row.id).await?;
                tx.commit().await?;
                continue;
            };
            let st = status_of(job_of(&j));
            let mut changed = false;
            if st == WorkflowStatus::InProgress && j.deadline_ms > 0 && j.deadline_ms <= now {
                j.cancel_requested = true;
                j.force_cancel = true;
                j.error = format!(
                    "The job exceeded the maximum execution time of {} minutes.",
                    j.timeout_ms / 60_000
                );
                j.result = "timed_out".into();
                self.finish_job(
                    &mut tx,
                    &idx.repo_id,
                    &mut j,
                    WorkflowConclusion::TimedOut,
                    "",
                    &mut fx,
                )
                .await?;
                changed = true;
            } else if st == WorkflowStatus::InProgress
                && j.lease_expires_ms > 0
                && j.lease_expires_ms <= now
            {
                if j.requeues < self.config.max_requeues {
                    self.requeue(&mut tx, &idx.repo_id, &mut j, &mut fx).await?;
                } else {
                    j.error = "The runner lost communication with the server.".into();
                    self.finish_job(
                        &mut tx,
                        &idx.repo_id,
                        &mut j,
                        WorkflowConclusion::Failure,
                        "",
                        &mut fx,
                    )
                    .await?;
                }
                changed = true;
            } else if st == WorkflowStatus::Waiting && j.wait_until_ms > 0 && j.wait_until_ms <= now
            {
                if let Some((rec, _)) = tx
                    .get_msg::<RunRecord>(&RUNS, &idx.repo_id, &idx.run_id)
                    .await?
                {
                    if j.env_state == super::environments::GREEN_WAIT {
                        // Held for `require_green`: run the environment gate again.
                        j.wait_until_ms = 0;
                        self.enqueue_job(&mut tx, &idx.repo_id, &rec, &mut j, &mut fx)
                            .await?;
                    } else if self.wait_elapsed(&mut tx, &idx.repo_id, &mut j).await? {
                        j.env_state = "approved".into();
                        self.enqueue_job(&mut tx, &idx.repo_id, &rec, &mut j, &mut fx)
                            .await?;
                    } else {
                        tx.put(&JOB_INDEX, &Row::new("", row.id.clone()).msg(&idx))
                            .await?;
                    }
                    changed = true;
                }
            } else {
                let next = next_attention(&j);
                tx.put(
                    &JOB_INDEX,
                    &match next {
                        Some(n) if n > now => Row::new("", row.id.clone()).n1(n as i64),
                        _ => Row::new("", row.id.clone()),
                    }
                    .msg(&idx),
                )
                .await?;
            }
            if changed {
                tx.update(&JOBS, &job_row(&idx.repo_id, &j).version(jrow.version))
                    .await?;
            }
            tx.commit().await?;
            fx.sync.insert((idx.repo_id.clone(), idx.run_id.clone()));
            drop(guard);
            self.apply(cx, fx).await;
        }
        self.expire_artifacts(cx).await?;
        self.expire_caches(cx).await?;
        self.expire_tokens(cx).await?;
        Ok(())
    }

    /// Puts a job whose runner vanished back in the queue; its log starts over.
    pub(crate) async fn requeue(
        &self,
        tx: &mut Tx,
        repo_id: &str,
        j: &mut JobRecord,
        fx: &mut Effects,
    ) -> Result<()> {
        let job_id = job_of(j).id.clone();
        if !j.runner_id.is_empty()
            && let Some((mut r, row)) = tx
                .get_msg::<RunnerRecord>(&RUNNERS, "", &j.runner_id)
                .await?
        {
            r.jobs.retain(|x| *x != job_id);
            let mut runner = r.runner.clone().unwrap_or_default();
            runner.busy = !r.jobs.is_empty();
            runner.current_job_id = r.jobs.first().cloned().unwrap_or_default();
            r.runner = Some(runner);
            tx.update(
                &RUNNERS,
                &super::runners::runner_row(&r).version(row.version),
            )
            .await?;
        }
        for row in tx
            .scan(
                &super::LOGS,
                &Scan::scope(job_id.clone(), crate::store::MAX_SCAN),
            )
            .await?
        {
            tx.delete(&super::LOGS, &job_id, &row.id).await?;
        }
        j.requeues += 1;
        j.runner_id.clear();
        j.lease_id.clear();
        j.lease_expires_ms = 0;
        j.deadline_ms = 0;
        j.secrets_redeemed = false;
        let mut job = job_of(j).clone();
        job.runner_id.clear();
        job.runner_name.clear();
        job.started_at = None;
        job.log_size_bytes = 0;
        for s in &mut job.steps {
            s.status = WorkflowStatus::Queued as i32;
            s.conclusion = 0;
            s.started_at = None;
            s.completed_at = None;
            s.log_offset = 0;
        }
        j.job = Some(job);
        let run_id = job_of(j).run_id.clone();
        let rec = tx
            .get_msg::<RunRecord>(&RUNS, repo_id, &run_id)
            .await?
            .map(|(r, _)| r)
            .unwrap_or_default();
        self.enqueue_job(tx, repo_id, &rec, j, fx).await?;
        tx.audit(
            AuditDraft::new("workflow.job_requeue", &j.org_id)
                .repo_id(repo_id)
                .target(SubjectKind::WorkflowRun, &run_id)
                .after(&serde_json::json!({ "job": job_id, "requeues": j.requeues }).to_string()),
        )
        .await?;
        Ok(())
    }

    /// Mints a job token: a Biscuit for the job principal `job:<id>`, limited to the repo and
    /// the job's lifetime, attenuated to the actions its `permissions` grant.
    pub(crate) fn mint_job_token(
        &self,
        job_id: &str,
        repo_id: &str,
        perms: &[String],
        expires_ms: u64,
    ) -> Result<(String, String)> {
        let now_s = self.now() / 1000;
        let principal = bee_auth::Principal::service(format!("job:{job_id}"), "github-actions");
        let base = self.signer.mint(
            &MintRequest {
                principal,
                on_behalf_of: Vec::new(),
                scope: Scope::new()
                    .repos([repo_id])?
                    .expires_at(expires_ms.div_ceil(1000)),
            },
            now_s,
        )?;
        let att = bee_auth::attenuate(
            &base.text,
            &Attenuation {
                scope: Scope::new().actions(token_actions(perms)),
                act_as: None,
            },
        )?;
        Ok((att.text, att.id.0))
    }
}

/// What to re-run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rerun {
    /// Every job.
    All,
    /// Failed and cancelled jobs and their dependents.
    Failed,
    /// One job (by id) and its dependents.
    Job(String),
}

/// Levels of remote reusable workflows fetched for one run.
pub const MAX_LEVELS: usize = 10;

fn merge(into: &mut Effects, more: Effects) {
    into.advance.extend(more.advance);
    into.cancel_runs.extend(more.cancel_runs);
    into.start_runs.extend(more.start_runs);
    into.sync.extend(more.sync);
    into.completed.extend(more.completed);
    into.wake |= more.wake;
}

fn planning_inputs(p: &Planning, wf: &Workflow) -> Object {
    if p.event.name == "workflow_dispatch" {
        let given = match &p.event.subject {
            bee_workflow::event::FilterSubject::Dispatch(_, i) => i.clone(),
            _ => BTreeMap::new(),
        };
        bee_workflow::event::dispatch_inputs(wf, &given)
            .map(|(typed, _)| typed)
            .unwrap_or_default()
    } else {
        Object::new()
    }
}

fn needs_json(needs: &BTreeMap<String, JobOutcome>) -> serde_json::Value {
    serde_json::Value::Object(
        needs
            .iter()
            .map(|(k, o)| {
                (
                    k.clone(),
                    serde_json::json!({
                        "result": o.result.as_str(),
                        "outputs": super::util::to_json(&Value::from(o.outputs.clone())),
                    }),
                )
            })
            .collect(),
    )
}

fn needs_from_json(text: &str) -> BTreeMap<String, JobOutcome> {
    let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let mut out = BTreeMap::new();
    if let Some(o) = v.as_object() {
        for (k, x) in o {
            let result = match x.get("result").and_then(|r| r.as_str()) {
                Some("success") => JobResult::Success,
                Some("cancelled") => JobResult::Cancelled,
                Some("skipped") => JobResult::Skipped,
                _ => JobResult::Failure,
            };
            let outputs = x
                .get("outputs")
                .map(super::util::from_json)
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            out.insert(k.clone(), JobOutcome { result, outputs });
        }
    }
    out
}

/// Aggregated outcomes of finished graph jobs (all legs completed), keyed by lower-case id.
fn outcomes_of(jobs: &[&JobRecord]) -> BTreeMap<String, JobOutcome> {
    let mut by_key: BTreeMap<String, Vec<&JobRecord>> = BTreeMap::new();
    for j in jobs {
        by_key.entry(j.key.to_lowercase()).or_default().push(j);
    }
    let mut out = BTreeMap::new();
    for (k, mut legs) in by_key {
        let total = legs.first().map(|l| l.legs.max(1)).unwrap_or(1) as usize;
        if legs.len() < total
            || legs
                .iter()
                .any(|l| status_of(job_of(l)) != WorkflowStatus::Completed)
        {
            continue;
        }
        legs.sort_by_key(|l| l.completed_seq);
        let leg_outcomes: Vec<LegOutcome> = legs
            .iter()
            .map(|l| LegOutcome {
                result: result_of(conclusion_of(&l.result)),
                continue_on_error: l.continue_on_error,
                outputs: super::convert::kv_object(&l.outputs),
            })
            .collect();
        out.insert(k, aggregate_legs(&leg_outcomes));
    }
    out
}

fn scope_result(outs: &BTreeMap<String, JobOutcome>, cancelled: bool) -> WorkflowConclusion {
    if cancelled {
        return WorkflowConclusion::Cancelled;
    }
    if outs.values().any(|o| o.result == JobResult::Failure) {
        WorkflowConclusion::Failure
    } else if outs.values().any(|o| o.result == JobResult::Cancelled) {
        WorkflowConclusion::Cancelled
    } else {
        WorkflowConclusion::Success
    }
}

/// `on.workflow_call.outputs` evaluated over the called workflow's job outcomes.
fn call_outputs(wf: &Workflow, outs: &BTreeMap<String, JobOutcome>) -> Vec<KeyValue> {
    let Some(t) = wf.trigger("workflow_call") else {
        return Vec::new();
    };
    let mut jobs = Object::new();
    for j in &wf.jobs {
        if let Some(o) = outs.get(&j.id.to_lowercase()) {
            jobs.insert(
                j.id.clone(),
                Value::object([
                    ("result", Value::from(o.result.as_str())),
                    ("outputs", Value::from(o.outputs.clone())),
                ]),
            );
        }
    }
    let ctx = bee_workflow::expr::MapContext::default().with("jobs", Value::from(jobs));
    t.outputs
        .iter()
        .map(|o| KeyValue {
            key: o.id.clone(),
            value: o.value.evaluate_string(&ctx).unwrap_or_default(),
        })
        .collect()
}

fn set_status(j: &mut JobRecord, s: WorkflowStatus, _now: u64) {
    let mut job = job_of(j).clone();
    job.status = s as i32;
    j.job = Some(job);
}

/// The next completion number of a run (matrix legs aggregate in completion order).
async fn next_seq(tx: &mut Tx, run_id: &str) -> Result<u64> {
    Ok(tx.add_n1(&COUNTERS, run_id, "completed_seq", 1).await? as u64)
}

fn set_done(j: &mut JobRecord, c: WorkflowConclusion, now: u64, seq: u64) {
    j.completed_seq = seq;
    let mut job = job_of(j).clone();
    job.status = WorkflowStatus::Completed as i32;
    job.conclusion = c as i32;
    job.completed_at = Some(ts(now));
    j.job = Some(job);
}

fn next_attention(j: &JobRecord) -> Option<u64> {
    [j.lease_expires_ms, j.deadline_ms, j.wait_until_ms]
        .into_iter()
        .filter(|&t| t > 0)
        .min()
}

/// The row of a run.
pub(crate) fn run_row(repo_id: &str, rec: &RunRecord) -> Row {
    let run = run_of(rec);
    let mut r = Row::new(repo_id, run.id.clone())
        .k1(run.workflow_id.clone())
        .n1(run.created_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
        .msg(rec);
    if let Some(c) = &run.head_changeset {
        r = r.k2(hex(&c.raw));
    }
    r
}

/// The row of a job.
pub(crate) fn job_row(repo_id: &str, rec: &JobRecord) -> Row {
    let job = job_of(rec);
    Row::new(repo_id, job.id.clone())
        .k1(job.run_id.clone())
        .n1(job.created_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
        .msg(rec)
}

/// Grants the job principal `job:<id>` a repo role while its job runs: write when its
/// permissions include any write scope, else read. The token's actions narrow it further.
pub struct JobAccess {
    db: Db,
}

#[async_trait]
impl AccessSource for JobAccess {
    async fn grants(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(RepoRole, String)>> {
        let Some(job_id) = principal_id.strip_prefix("job:") else {
            return Ok(Vec::new());
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((idx, _)) = tx.get_msg::<JobIndex>(&JOB_INDEX, "", job_id).await? else {
            return Ok(Vec::new());
        };
        if idx.repo_id != repo.id {
            return Ok(Vec::new());
        }
        let Some((j, _)) = tx.get_msg::<JobRecord>(&JOBS, &repo.id, job_id).await? else {
            return Ok(Vec::new());
        };
        if status_of(job_of(&j)) != WorkflowStatus::InProgress {
            return Ok(Vec::new());
        }
        let role = if j.permissions.iter().any(|p| p.ends_with(":write")) {
            RepoRole::Write
        } else {
            RepoRole::Read
        };
        Ok(vec![(role, "workflow_job".into())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_paths() {
        assert!(is_ci_workflow_path(".github/workflows/ci.yml"));
        assert!(is_ci_workflow_path(".beehive/workflows/ci.yaml"));
        assert!(!is_ci_workflow_path(".github/workflows/sub/ci.yml"));
        assert!(!is_ci_workflow_path(".github/workflows/ci.txt"));
        assert!(!is_ci_workflow_path("x/.github/workflows/ci.yml"));
    }
}
