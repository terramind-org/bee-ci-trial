//! Crate-private row records of the ci surface (protobuf, additive-only like the wire
//! contract). Public wire messages (`WorkflowRun`, `WorkflowJob`, `Runner`, ...) are embedded
//! where they exist; these records add what the server needs and never returns.

use bee_proto::v1::{
    Attestation, Deployment, DeploymentEnvironment, DeploymentReview, DeploymentStatus, KeyValue,
    Runner, RunnerGroup, RunnerImage, Secret, Variable, Workflow, WorkflowArtifact, WorkflowCache,
    WorkflowJob, WorkflowOidcClaimTemplate, WorkflowRequirement, WorkflowRun,
};
use prost::Message;

/// A workflow file seen in a repo.
#[derive(Clone, PartialEq, Message)]
pub struct WorkflowRow {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub workflow: Option<Workflow>,
    /// Cron entries of the default-ref version.
    #[prost(string, repeated, tag = "2")]
    pub schedules: Vec<String>,
    /// The repo's org.
    #[prost(string, tag = "3")]
    pub org_id: String,
}

/// An `ActionsEvent` in storable form (see [`crate::ci::convert`]).
#[derive(Clone, PartialEq, Message)]
pub struct StoredEvent {
    /// `github.event_name`.
    #[prost(string, tag = "1")]
    pub name: String,
    /// Activity type.
    #[prost(string, tag = "2")]
    pub action: String,
    /// Whether `action` is set.
    #[prost(bool, tag = "3")]
    pub has_action: bool,
    /// `GITHUB_REF`.
    #[prost(string, tag = "4")]
    pub git_ref: String,
    /// `GITHUB_SHA`.
    #[prost(string, tag = "5")]
    pub sha: String,
    /// `GITHUB_BASE_REF`.
    #[prost(string, tag = "6")]
    pub base_ref: String,
    /// `GITHUB_HEAD_REF`.
    #[prost(string, tag = "7")]
    pub head_ref: String,
    /// `github.event` as JSON.
    #[prost(string, tag = "8")]
    pub payload_json: String,
    /// `github.actor`.
    #[prost(string, tag = "9")]
    pub actor: String,
    /// Filter subject kind: `none`, `branch`, `tag`, `pull_request`, `merge_group`,
    /// `workflow_run`, `schedule`, `dispatch`.
    #[prost(string, tag = "10")]
    pub subject: String,
    /// First subject string (branch, tag, base, workflow name, dispatch path).
    #[prost(string, tag = "11")]
    pub subject_a: String,
    /// Second subject string (`workflow_run` head branch).
    #[prost(string, tag = "12")]
    pub subject_b: String,
    /// Changed paths.
    #[prost(string, repeated, tag = "13")]
    pub paths: Vec<String>,
    /// Whether `paths` is known.
    #[prost(bool, tag = "14")]
    pub paths_known: bool,
    /// `workflow_run` chain depth.
    #[prost(uint32, tag = "15")]
    pub depth: u32,
    /// Schedule minute: year, month, day, hour, minute.
    #[prost(int64, repeated, tag = "16")]
    pub minute: Vec<i64>,
    /// Dispatch inputs.
    #[prost(message, repeated, tag = "17")]
    pub inputs: Vec<KeyValue>,
    /// Skip-CI marker present.
    #[prost(bool, tag = "18")]
    pub skip_ci: bool,
    /// Caused by a workflow token.
    #[prost(bool, tag = "19")]
    pub by_workflow_token: bool,
}

/// Facts about the repo as the run saw them (`RepoInfo`).
#[derive(Clone, PartialEq, Message)]
pub struct RepoInfoRecord {
    /// Owner login.
    #[prost(string, tag = "1")]
    pub owner: String,
    /// Repo name.
    #[prost(string, tag = "2")]
    pub name: String,
    /// Default branch.
    #[prost(string, tag = "3")]
    pub default_branch: String,
    /// Default branch tip (hex).
    #[prost(string, tag = "4")]
    pub default_branch_sha: String,
    /// Web URL.
    #[prost(string, tag = "5")]
    pub server_url: String,
    /// REST URL.
    #[prost(string, tag = "6")]
    pub api_url: String,
    /// GraphQL URL.
    #[prost(string, tag = "7")]
    pub graphql_url: String,
}

/// A graph scope of a run: the top-level workflow (`id == ""`) or one leg of a reusable
/// workflow call.
#[derive(Clone, PartialEq, Message)]
pub struct ScopeRecord {
    /// Scope id: `""` or `<parent>/<caller job>#<leg>`.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Key of the workflow source in the run's file snapshot.
    #[prost(string, tag = "2")]
    pub workflow_key: String,
    /// Parent scope id.
    #[prost(string, tag = "3")]
    pub parent: String,
    /// The calling job's id in the parent scope.
    #[prost(string, tag = "4")]
    pub caller: String,
    /// The calling job's matrix leg.
    #[prost(uint32, tag = "5")]
    pub leg: u32,
    /// `inputs` context of the called workflow (JSON).
    #[prost(string, tag = "6")]
    pub inputs_json: String,
    /// `secrets: inherit`.
    #[prost(bool, tag = "7")]
    pub secrets_inherit: bool,
    /// Explicit secrets: name → template (JSON parts).
    #[prost(message, repeated, tag = "8")]
    pub secrets: Vec<KeyValue>,
    /// The scope finished and its caller job was completed.
    #[prost(bool, tag = "9")]
    pub done: bool,
    /// Display prefix of the scope's jobs (`caller / `).
    #[prost(string, tag = "10")]
    pub prefix: String,
    /// Outcomes of the calling job's needs (JSON), for re-planning the call.
    #[prost(string, tag = "11")]
    pub caller_needs_json: String,
}

/// A run and everything needed to keep planning it.
#[derive(Clone, PartialEq, Message)]
pub struct RunRecord {
    /// The public view of the latest attempt.
    #[prost(message, optional, tag = "1")]
    pub run: Option<WorkflowRun>,
    /// The triggering event.
    #[prost(message, optional, tag = "2")]
    pub event: Option<StoredEvent>,
    /// Workflow sources the run uses: path (or remote reference) → YAML.
    #[prost(message, repeated, tag = "3")]
    pub files: Vec<KeyValue>,
    /// The `vars` context (JSON).
    #[prost(string, tag = "4")]
    pub vars_json: String,
    /// `github.run_id`.
    #[prost(uint64, tag = "5")]
    pub numeric_id: u64,
    /// Matching cron entry.
    #[prost(string, tag = "6")]
    pub schedule: String,
    /// Reusable-workflow scopes (the top-level scope is implicit).
    #[prost(message, repeated, tag = "7")]
    pub scopes: Vec<ScopeRecord>,
    /// Cancel requested.
    #[prost(bool, tag = "8")]
    pub cancelled: bool,
    /// Force cancel requested.
    #[prost(bool, tag = "9")]
    pub force_cancelled: bool,
    /// The repo's org.
    #[prost(string, tag = "10")]
    pub org_id: String,
    /// The repo as planned.
    #[prost(message, optional, tag = "11")]
    pub repo_info: Option<RepoInfoRecord>,
    /// Workflow concurrency group key (lower case), empty when none.
    #[prost(string, tag = "12")]
    pub concurrency_key: String,
    /// `cancel-in-progress`.
    #[prost(bool, tag = "13")]
    pub concurrency_cancel: bool,
    /// `queue: max`.
    #[prost(bool, tag = "14")]
    pub concurrency_queue_max: bool,
    /// Required workflow id when the run enforces one.
    #[prost(string, tag = "15")]
    pub required_workflow_id: String,
    /// Completion counter for matrix leg ordering.
    #[prost(uint64, tag = "16")]
    pub completed_seq: u64,
    /// Changeset check runs are reported on (hex).
    #[prost(string, tag = "17")]
    pub check_changeset: String,
    /// The run failed before any job (invalid workflow, policy): the diagnostics.
    #[prost(string, tag = "18")]
    pub startup_error: String,
    /// Whether the cause was a workflow token.
    #[prost(bool, tag = "19")]
    pub by_workflow_token: bool,
    /// Base ref of a change request run (for cache scopes).
    #[prost(string, tag = "20")]
    pub base_ref: String,
    /// Path the workflow came from (the run's workflow key).
    #[prost(string, tag = "21")]
    pub workflow_key: String,
    /// The run was released from its concurrency group.
    #[prost(bool, tag = "22")]
    pub concurrency_released: bool,
    /// Chain depth of `workflow_run` triggers that led here.
    #[prost(uint32, tag = "23")]
    pub chain_depth: u32,
    /// The repo was public when the run was created.
    #[prost(bool, tag = "24")]
    pub repo_public: bool,
    /// Probe runs only: top-level job keys (lower case) allowed to run, the target and
    /// everything it needs; every other job is skipped.
    #[prost(string, repeated, tag = "25")]
    pub only_jobs: Vec<String>,
    /// A bisect probe: reports no check runs, joins no concurrency group and starts no
    /// `workflow_run` chain.
    #[prost(bool, tag = "26")]
    pub probe: bool,
    /// The run checks a change request whose head comes from a fork: its jobs get no
    /// secrets unless the actions policy's `fork_send_secrets` allows them.
    #[prost(bool, tag = "27")]
    pub from_fork: bool,
}

/// A job (one matrix leg, one reusable-workflow call leg, or a skipped job).
#[derive(Clone, PartialEq, Message)]
pub struct JobRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub job: Option<WorkflowJob>,
    /// The repo's org.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Graph scope id.
    #[prost(string, tag = "3")]
    pub scope: String,
    /// Job id in its workflow.
    #[prost(string, tag = "4")]
    pub key: String,
    /// Matrix leg index.
    #[prost(uint32, tag = "5")]
    pub leg: u32,
    /// Number of legs.
    #[prost(uint32, tag = "6")]
    pub legs: u32,
    /// The resolved job for the runner (see [`crate::ci::convert::plan_json`]).
    #[prost(string, tag = "7")]
    pub plan_json: String,
    /// `step` (runs on a runner), `call` (a reusable workflow leg) or `skip`.
    #[prost(string, tag = "8")]
    pub kind: String,
    /// Current lease.
    #[prost(string, tag = "9")]
    pub lease_id: String,
    /// Lease expiry (ms).
    #[prost(uint64, tag = "10")]
    pub lease_expires_ms: u64,
    /// Runner holding the lease.
    #[prost(string, tag = "11")]
    pub runner_id: String,
    /// Job outputs.
    #[prost(message, repeated, tag = "12")]
    pub outputs: Vec<KeyValue>,
    /// The leg's own result: `success`, `failure`, `cancelled`, `skipped`.
    #[prost(string, tag = "13")]
    pub result: String,
    /// `continue-on-error`.
    #[prost(bool, tag = "14")]
    pub continue_on_error: bool,
    /// Completion order within the run.
    #[prost(uint64, tag = "15")]
    pub completed_seq: u64,
    /// Cancellation requested.
    #[prost(bool, tag = "16")]
    pub cancel_requested: bool,
    /// Force cancellation requested.
    #[prost(bool, tag = "17")]
    pub force_cancel: bool,
    /// Timeout (ms).
    #[prost(uint64, tag = "18")]
    pub timeout_ms: u64,
    /// Absolute deadline once started (ms).
    #[prost(uint64, tag = "19")]
    pub deadline_ms: u64,
    /// Step summary markdown.
    #[prost(string, tag = "20")]
    pub summary: String,
    /// Held back (max-parallel or a pending concurrency group).
    #[prost(bool, tag = "21")]
    pub held: bool,
    /// Environment name.
    #[prost(string, tag = "22")]
    pub environment: String,
    /// Deployment for the environment.
    #[prost(string, tag = "23")]
    pub deployment_id: String,
    /// `none`, `waiting`, `approved`, `rejected`.
    #[prost(string, tag = "24")]
    pub env_state: String,
    /// Wait timer end (ms).
    #[prost(uint64, tag = "25")]
    pub wait_until_ms: u64,
    /// Times the job was requeued after a lost lease.
    #[prost(uint32, tag = "26")]
    pub requeues: u32,
    /// Last check status synced to the checks module.
    #[prost(int32, tag = "27")]
    pub check_synced: i32,
    /// Effective permissions as `scope:access` (`contents:read`, `id-token:write`).
    #[prost(string, repeated, tag = "28")]
    pub permissions: Vec<String>,
    /// Job concurrency group key.
    #[prost(string, tag = "29")]
    pub concurrency_key: String,
    /// Job `cancel-in-progress`.
    #[prost(bool, tag = "30")]
    pub concurrency_cancel: bool,
    /// Job `queue: max`.
    #[prost(bool, tag = "31")]
    pub concurrency_queue_max: bool,
    /// Copied from the previous attempt by a partial re-run.
    #[prost(bool, tag = "32")]
    pub carried: bool,
    /// `strategy.fail-fast`.
    #[prost(bool, tag = "33")]
    pub fail_fast: bool,
    /// `strategy.max-parallel`.
    #[prost(uint32, tag = "34")]
    pub max_parallel: u32,
    /// `runs-on.group`.
    #[prost(string, tag = "35")]
    pub group: String,
    /// The secrets grant was redeemed.
    #[prost(bool, tag = "36")]
    pub secrets_redeemed: bool,
    /// One-time secret grant nonce.
    #[prost(string, tag = "37")]
    pub secrets_nonce: String,
    /// The job is released from its concurrency group.
    #[prost(bool, tag = "38")]
    pub concurrency_released: bool,
    /// Failure explanation shown in the log and the check run.
    #[prost(string, tag = "39")]
    pub error: String,
    /// The job token's id.
    #[prost(string, tag = "40")]
    pub token_id: String,
    /// The job's logs were deleted.
    #[prost(bool, tag = "41")]
    pub logs_deleted: bool,
    /// The evaluated `environment.url` a native runner reported on completion.
    #[prost(string, tag = "42")]
    pub environment_url: String,
}

/// A job waiting in the queue.
#[derive(Clone, PartialEq, Message)]
pub struct QueueEntry {
    /// Repo.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// Org.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// `runs-on` labels.
    #[prost(string, repeated, tag = "3")]
    pub labels: Vec<String>,
    /// `runs-on.group`.
    #[prost(string, tag = "4")]
    pub group: String,
    /// The repo is public.
    #[prost(bool, tag = "5")]
    pub public: bool,
    /// Workflow path, for groups restricted to workflows.
    #[prost(string, tag = "6")]
    pub workflow_ref: String,
}

/// Where an unfinished job lives.
#[derive(Clone, PartialEq, Message)]
pub struct JobIndex {
    /// Repo.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// Org.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Run.
    #[prost(string, tag = "3")]
    pub run_id: String,
}

/// One stored log chunk.
#[derive(Clone, PartialEq, Message)]
pub struct LogChunk {
    /// Byte offset in the job log.
    #[prost(uint64, tag = "1")]
    pub offset: u64,
    /// Length in bytes.
    #[prost(uint64, tag = "2")]
    pub len: u64,
    /// Step number.
    #[prost(uint32, tag = "3")]
    pub step: u32,
    /// Blob holding the bytes.
    #[prost(string, tag = "4")]
    pub blob_id: String,
    /// The org (blob namespace).
    #[prost(string, tag = "5")]
    pub org_id: String,
}

/// A runner.
#[derive(Clone, PartialEq, Message)]
pub struct RunnerRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub runner: Option<Runner>,
    /// Owning org.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Owning repo (repo-level runners).
    #[prost(string, tag = "3")]
    pub repo_id: String,
    /// Credential token id.
    #[prost(string, tag = "4")]
    pub credential_id: String,
    /// Jobs it holds.
    #[prost(string, repeated, tag = "5")]
    pub jobs: Vec<String>,
    /// Last heartbeat (ms).
    #[prost(uint64, tag = "6")]
    pub last_seen_ms: u64,
    /// An ephemeral runner already took its job.
    #[prost(bool, tag = "7")]
    pub spent: bool,
}

/// A runner group.
#[derive(Clone, PartialEq, Message)]
pub struct GroupRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub group: Option<RunnerGroup>,
    /// Repos allowed with visibility SELECTED.
    #[prost(string, repeated, tag = "2")]
    pub repo_ids: Vec<String>,
}

/// A registration or removal token.
#[derive(Clone, PartialEq, Message)]
pub struct TokenRecord {
    /// `register` or `remove`.
    #[prost(string, tag = "1")]
    pub kind: String,
    /// Org.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Repo (repo-level tokens).
    #[prost(string, tag = "3")]
    pub repo_id: String,
    /// Expiry (ms).
    #[prost(uint64, tag = "4")]
    pub expires_ms: u64,
}

/// A custom runner image.
#[derive(Clone, PartialEq, Message)]
pub struct ImageRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub image: Option<RunnerImage>,
}

/// An artifact.
#[derive(Clone, PartialEq, Message)]
pub struct ArtifactRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub artifact: Option<WorkflowArtifact>,
    /// Blob with the bytes once uploaded.
    #[prost(string, tag = "2")]
    pub blob_id: String,
    /// Org (blob namespace).
    #[prost(string, tag = "3")]
    pub org_id: String,
    /// Uploaded size.
    #[prost(uint64, tag = "4")]
    pub uploaded_size: u64,
    /// Uploaded digest (`sha256:<hex>`).
    #[prost(string, tag = "5")]
    pub uploaded_digest: String,
}

/// A cache entry.
#[derive(Clone, PartialEq, Message)]
pub struct CacheRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub cache: Option<WorkflowCache>,
    /// Blob with the bytes once uploaded.
    #[prost(string, tag = "2")]
    pub blob_id: String,
    /// Org (blob namespace).
    #[prost(string, tag = "3")]
    pub org_id: String,
    /// Uploaded digest.
    #[prost(string, tag = "4")]
    pub uploaded_digest: String,
    /// Uploaded size.
    #[prost(uint64, tag = "5")]
    pub uploaded_size: u64,
}

/// A secret: metadata and the value sealed with the server key.
#[derive(Clone, PartialEq, Message)]
pub struct SecretRecord {
    /// Metadata (never carries the value).
    #[prost(message, optional, tag = "1")]
    pub secret: Option<Secret>,
    /// AEAD nonce.
    #[prost(bytes = "vec", tag = "2")]
    pub nonce: Vec<u8>,
    /// AEAD ciphertext of the UTF-8 value.
    #[prost(bytes = "vec", tag = "3")]
    pub ciphertext: Vec<u8>,
    /// Repos allowed with visibility SELECTED.
    #[prost(string, repeated, tag = "4")]
    pub repo_ids: Vec<String>,
}

/// A variable.
#[derive(Clone, PartialEq, Message)]
pub struct VariableRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub variable: Option<Variable>,
    /// Repos allowed with visibility SELECTED.
    #[prost(string, repeated, tag = "2")]
    pub repo_ids: Vec<String>,
}

/// An environment.
#[derive(Clone, PartialEq, Message)]
pub struct EnvironmentRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub environment: Option<DeploymentEnvironment>,
}

/// A deployment with its history.
#[derive(Clone, PartialEq, Message)]
pub struct DeploymentRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub deployment: Option<Deployment>,
    /// Statuses, oldest first.
    #[prost(message, repeated, tag = "2")]
    pub statuses: Vec<DeploymentStatus>,
    /// Review decisions, oldest first.
    #[prost(message, repeated, tag = "3")]
    pub reviews: Vec<DeploymentReview>,
    /// The workflow job deploying.
    #[prost(string, tag = "4")]
    pub job_id: String,
    /// Waiting on reviewers.
    #[prost(bool, tag = "5")]
    pub pending: bool,
    /// Wait timer start (ms), 0 when not started.
    #[prost(uint64, tag = "6")]
    pub wait_started_ms: u64,
    /// Apps of custom protection rules that have not answered yet.
    #[prost(string, repeated, tag = "7")]
    pub rule_pending: Vec<String>,
}

/// An actions policy with its OIDC template.
#[derive(Clone, PartialEq, Message)]
pub struct PolicyRecord {
    /// The stored values (`scope` set).
    #[prost(message, optional, tag = "1")]
    pub policy: Option<bee_proto::v1::ActionsPolicy>,
    /// Which fields were set explicitly at this level.
    #[prost(string, repeated, tag = "2")]
    pub set_fields: Vec<String>,
    /// Repos enabled with SELECTED (org policies).
    #[prost(string, repeated, tag = "3")]
    pub enabled_repo_ids: Vec<String>,
    /// OIDC subject template.
    #[prost(message, optional, tag = "4")]
    pub oidc: Option<WorkflowOidcClaimTemplate>,
}

/// A required workflow.
#[derive(Clone, PartialEq, Message)]
pub struct RequiredRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub requirement: Option<WorkflowRequirement>,
    /// Source repo id.
    #[prost(string, tag = "2")]
    pub source_repo_id: String,
    /// Selected repo ids.
    #[prost(string, repeated, tag = "3")]
    pub repo_ids: Vec<String>,
}

/// An attestation.
#[derive(Clone, PartialEq, Message)]
pub struct AttestationRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub attestation: Option<Attestation>,
    /// Org.
    #[prost(string, tag = "2")]
    pub org_id: String,
}

/// Concurrency group membership, oldest first.
#[derive(Clone, PartialEq, Message)]
pub struct ConcurrencyGroup {
    /// Members.
    #[prost(message, repeated, tag = "1")]
    pub members: Vec<ConcurrencyMember>,
}

/// One member of a concurrency group.
#[derive(Clone, PartialEq, Message)]
pub struct ConcurrencyMember {
    /// Run id, or `job:<job id>`.
    #[prost(string, tag = "1")]
    pub id: String,
    /// In progress (otherwise pending).
    #[prost(bool, tag = "2")]
    pub running: bool,
    /// The member's run id.
    #[prost(string, tag = "3")]
    pub run_id: String,
}

/// Last tip of a ref.
#[derive(Clone, PartialEq, Message)]
pub struct RefTip {
    /// Tip (hex), empty when deleted.
    #[prost(string, tag = "1")]
    pub target: String,
}

/// Metered job time.
#[derive(Clone, PartialEq, Message)]
pub struct UsageRecord {
    /// Job.
    #[prost(string, tag = "1")]
    pub job_id: String,
    /// Run.
    #[prost(string, tag = "2")]
    pub run_id: String,
    /// Workflow.
    #[prost(string, tag = "3")]
    pub workflow_id: String,
    /// Machine class (`self-hosted` for self-hosted runners).
    #[prost(string, tag = "4")]
    pub machine_class: String,
    /// Wall time.
    #[prost(uint64, tag = "5")]
    pub duration_ms: u64,
    /// Billable time (hosted runners, rounded up to whole minutes).
    #[prost(uint64, tag = "6")]
    pub billable_ms: u64,
    /// Completion time.
    #[prost(uint64, tag = "7")]
    pub completed_ms: u64,
}

/// A recorded approval.
#[derive(Clone, PartialEq, Message)]
pub struct ApprovalRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub approval: Option<bee_proto::v1::WorkflowRunApproval>,
}
