//! Crate-private stored records. Wire resources (`bee.v1.AgentRun`, `Watcher`, `Anomaly`,
//! `Workspace`, ...) are stored inside these records as they are; the records add what the
//! wire messages do not carry.
//!
//! Records inherit the additive-only rule: never renumber or reuse a field.

use bee_proto::v1::{
    AgentFeedTrigger, AgentRun, AgentRunBudget, Cost, ObjectId, Principal, Watcher, Workspace,
    WorkspacePort, WorkspacePrebuild, WorkspacePrebuildConfig,
};
use prost::Message;

/// A run with its provenance and bookkeeping.
#[derive(Clone, PartialEq, Message)]
pub struct RunRec {
    /// The wire run.
    #[prost(message, optional, tag = "1")]
    pub run: Option<AgentRun>,
    /// Org of the repo.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// What started it ([`crate::agents::Trigger`] name).
    #[prost(string, tag = "3")]
    pub trigger: String,
    /// The trigger's subject (comment id, watcher id, schedule id, previous run id).
    #[prost(string, tag = "4")]
    pub trigger_ref: String,
    /// Delegation chain of the run token: agent first, the human or service root last.
    #[prost(string, repeated, tag = "5")]
    pub chain: Vec<String>,
    /// Id of the delegated token.
    #[prost(string, tag = "6")]
    pub token_id: String,
    /// Issue number for issue runs.
    #[prost(uint64, tag = "7")]
    pub issue_number: u64,
    /// BLAKE3 of the prompt the runner received.
    #[prost(bytes = "vec", tag = "8")]
    pub prompt_digest: Vec<u8>,
    /// Blob reference of the transcript.
    #[prost(string, tag = "9")]
    pub transcript_ref: String,
    /// Blob reference of the raw log.
    #[prost(string, tag = "10")]
    pub log_ref: String,
    /// Instructions file of the agent definition.
    #[prost(string, tag = "11")]
    pub instructions_path: String,
    /// Toolsets (policy actions) the token allows.
    #[prost(string, repeated, tag = "12")]
    pub toolsets: Vec<String>,
    /// Watcher that triggered the run.
    #[prost(string, tag = "13")]
    pub watcher_id: String,
    /// Run this one retries.
    #[prost(string, tag = "14")]
    pub retry_of: String,
    /// Idempotency key of the start request.
    #[prost(string, tag = "15")]
    pub idempotency_key: String,
    /// Owner of the agent whose budget pays.
    #[prost(string, tag = "16")]
    pub owner_id: String,
    /// Runtime name (`claude-code`, `codex`, ...).
    #[prost(string, tag = "17")]
    pub runtime_name: String,
    /// Steering messages not yet delivered to the runner.
    #[prost(string, repeated, tag = "18")]
    pub pending_messages: Vec<String>,
    /// Machine type of the run's workspace (empty = no workspace).
    #[prost(string, tag = "19")]
    pub machine_type: String,
    /// The run exceeded its budget while running.
    #[prost(bool, tag = "20")]
    pub over_budget: bool,
    /// Feed trigger that started the run (`<repo id>/<agent id>`); its daily counters pay.
    #[prost(string, tag = "21")]
    pub feed_trigger: String,
    /// ADOPT_EVALUATE: every post folded into the run, oldest first (`task.post_id` is the
    /// first).
    #[prost(string, repeated, tag = "22")]
    pub feed_post_ids: Vec<String>,
}

impl RunRec {
    /// The wire run (never absent in stored rows).
    pub fn run(&self) -> AgentRun {
        let mut run = self.run.clone().unwrap_or_default();
        run.transcript_ref.clone_from(&self.transcript_ref);
        run.log_ref.clone_from(&self.log_ref);
        run.trigger.clone_from(&self.trigger);
        run.trigger_ref.clone_from(&self.trigger_ref);
        run
    }
}

/// A scheduled trigger of an agent definition.
#[derive(Clone, PartialEq, Message)]
pub struct ScheduleRec {
    /// Schedule id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Repo the run works in.
    #[prost(string, tag = "2")]
    pub repo_id: String,
    /// Five-field cron expression (UTC).
    #[prost(string, tag = "3")]
    pub cron: String,
    /// The task prompt.
    #[prost(string, tag = "4")]
    pub prompt: String,
    /// When it last fired (ms).
    #[prost(uint64, tag = "5")]
    pub last_fired_ms: u64,
}

/// An agent definition (registry entry) bound to an agent principal.
#[derive(Clone, PartialEq, Message)]
pub struct DefRec {
    /// Org.
    #[prost(string, tag = "1")]
    pub org_id: String,
    /// The agent principal.
    #[prost(message, optional, tag = "2")]
    pub agent: Option<Principal>,
    /// Name, unique per org.
    #[prost(string, tag = "3")]
    pub name: String,
    /// Registered runtime id (empty = by name only).
    #[prost(string, tag = "4")]
    pub runtime_id: String,
    /// Runtime name, e.g. `claude-code`.
    #[prost(string, tag = "5")]
    pub runtime: String,
    /// Default model.
    #[prost(string, tag = "6")]
    pub model: String,
    /// Instructions file path in the repo.
    #[prost(string, tag = "7")]
    pub instructions_path: String,
    /// Allowed toolsets: policy action names.
    #[prost(string, repeated, tag = "8")]
    pub toolsets: Vec<String>,
    /// Default per-run budget.
    #[prost(message, optional, tag = "9")]
    pub default_budget: Option<AgentRunBudget>,
    /// Owners; the first is the primary owner whose budget pays.
    #[prost(string, repeated, tag = "10")]
    pub owners: Vec<String>,
    /// What the agent is for.
    #[prost(string, tag = "11")]
    pub purpose: String,
    /// Takes work.
    #[prost(bool, tag = "12")]
    pub enabled: bool,
    /// Created (ms).
    #[prost(uint64, tag = "13")]
    pub created_ms: u64,
    /// Updated (ms).
    #[prost(uint64, tag = "14")]
    pub updated_ms: u64,
    /// Schedules.
    #[prost(message, repeated, tag = "15")]
    pub schedules: Vec<ScheduleRec>,
}

/// Monthly limits of an owner's agents.
#[derive(Clone, PartialEq, Message)]
pub struct OwnerBudgetRec {
    /// Owner principal id.
    #[prost(string, tag = "1")]
    pub owner_id: String,
    /// USD micros per month (0 = unlimited).
    #[prost(uint64, tag = "2")]
    pub max_usd_micros: u64,
    /// Input tokens per month (0 = unlimited).
    #[prost(uint64, tag = "3")]
    pub max_input_tokens: u64,
    /// Output tokens per month (0 = unlimited).
    #[prost(uint64, tag = "4")]
    pub max_output_tokens: u64,
    /// Runs per month (0 = unlimited).
    #[prost(uint64, tag = "5")]
    pub max_runs: u64,
    /// Updated (ms).
    #[prost(uint64, tag = "6")]
    pub updated_ms: u64,
}

/// What an owner's agents spent in one month.
#[derive(Clone, PartialEq, Message)]
pub struct SpendRec {
    /// USD micros.
    #[prost(uint64, tag = "1")]
    pub usd_micros: u64,
    /// Input tokens.
    #[prost(uint64, tag = "2")]
    pub input_tokens: u64,
    /// Output tokens.
    #[prost(uint64, tag = "3")]
    pub output_tokens: u64,
    /// Runs started.
    #[prost(uint64, tag = "4")]
    pub runs: u64,
}

/// A provenance session: one run's model, prompt and the changes it produced.
#[derive(Clone, PartialEq, Message)]
pub struct SessionRec {
    /// Session id (also in the run token and in changeset provenance).
    #[prost(string, tag = "1")]
    pub session: String,
    /// Repo.
    #[prost(string, tag = "2")]
    pub repo_id: String,
    /// Org.
    #[prost(string, tag = "3")]
    pub org_id: String,
    /// Agent principal.
    #[prost(string, tag = "4")]
    pub agent_id: String,
    /// Run.
    #[prost(string, tag = "5")]
    pub run_id: String,
    /// Model.
    #[prost(string, tag = "6")]
    pub model: String,
    /// Runtime name.
    #[prost(string, tag = "7")]
    pub runtime: String,
    /// BLAKE3 of the prompt.
    #[prost(bytes = "vec", tag = "8")]
    pub prompt_digest: Vec<u8>,
    /// Delegation chain, agent first.
    #[prost(string, repeated, tag = "9")]
    pub chain: Vec<String>,
    /// Changesets produced.
    #[prost(message, repeated, tag = "10")]
    pub changesets: Vec<ObjectId>,
    /// Change request ids produced.
    #[prost(string, repeated, tag = "11")]
    pub change_request_ids: Vec<String>,
    /// Created (ms).
    #[prost(uint64, tag = "12")]
    pub created_ms: u64,
    /// Cost.
    #[prost(message, optional, tag = "13")]
    pub cost: Option<Cost>,
}

/// A change request an agent authored.
#[derive(Clone, PartialEq, Message)]
pub struct AgentCrRec {
    /// Repo.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// CR id.
    #[prost(string, tag = "2")]
    pub cr_id: String,
    /// Number.
    #[prost(uint64, tag = "3")]
    pub number: u64,
    /// Title.
    #[prost(string, tag = "4")]
    pub title: String,
    /// `open`, `landed`, `closed`.
    #[prost(string, tag = "5")]
    pub state: String,
    /// Opened (ms).
    #[prost(uint64, tag = "6")]
    pub created_ms: u64,
    /// Reverted by a later CR.
    #[prost(bool, tag = "7")]
    pub reverted: bool,
    /// Session that produced it, if a run did.
    #[prost(string, tag = "8")]
    pub session: String,
}

/// A review an agent submitted.
#[derive(Clone, PartialEq, Message)]
pub struct AgentReviewRec {
    /// Repo.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// CR id.
    #[prost(string, tag = "2")]
    pub cr_id: String,
    /// Verdict name.
    #[prost(string, tag = "3")]
    pub verdict: String,
    /// Submitted (ms).
    #[prost(uint64, tag = "4")]
    pub created_ms: u64,
}

/// A watcher with its bookkeeping.
#[derive(Clone, PartialEq, Message)]
pub struct WatcherRec {
    /// The wire watcher.
    #[prost(message, optional, tag = "1")]
    pub watcher: Option<Watcher>,
    /// Org.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Creator.
    #[prost(string, tag = "3")]
    pub created_by: String,
    /// UTC day of the counters below.
    #[prost(string, tag = "4")]
    pub day: String,
    /// Runs started that day.
    #[prost(uint64, tag = "5")]
    pub day_runs: u64,
    /// USD micros spent that day.
    #[prost(uint64, tag = "6")]
    pub day_usd_micros: u64,
}

impl WatcherRec {
    /// The wire watcher.
    pub fn w(&self) -> Watcher {
        self.watcher.clone().unwrap_or_default()
    }
}

/// A pending, debounced watcher trigger.
#[derive(Clone, PartialEq, Message)]
pub struct TriggerRec {
    /// Watcher.
    #[prost(string, tag = "1")]
    pub watcher_id: String,
    /// Repo.
    #[prost(string, tag = "2")]
    pub repo_id: String,
    /// Changed paths inside the watcher's scope.
    #[prost(string, repeated, tag = "3")]
    pub paths: Vec<String>,
    /// Changesets that changed them.
    #[prost(message, repeated, tag = "4")]
    pub changesets: Vec<ObjectId>,
    /// First change seen (ms).
    #[prost(uint64, tag = "5")]
    pub first_ms: u64,
    /// When the trigger fires (ms).
    #[prost(uint64, tag = "6")]
    pub due_ms: u64,
    /// Changes folded into this trigger.
    #[prost(uint64, tag = "7")]
    pub changes: u64,
}

/// An agent's feed trigger in a repo with its bookkeeping.
#[derive(Clone, PartialEq, Message)]
pub struct FeedTriggerRec {
    /// The wire trigger (counters and the check interval are filled on read).
    #[prost(message, optional, tag = "1")]
    pub trigger: Option<AgentFeedTrigger>,
    /// Org of the repo.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Who configured it last (the runs act on their behalf).
    #[prost(string, tag = "3")]
    pub configured_by: String,
    /// UTC day of the counters below.
    #[prost(string, tag = "4")]
    pub day: String,
    /// Runs started that day.
    #[prost(uint64, tag = "5")]
    pub day_runs: u64,
    /// USD micros spent that day.
    #[prost(uint64, tag = "6")]
    pub day_usd_micros: u64,
    /// Source repo ids, as followed for this trigger.
    #[prost(string, repeated, tag = "7")]
    pub source_ids: Vec<String>,
}

impl FeedTriggerRec {
    /// The wire trigger.
    pub fn t(&self) -> AgentFeedTrigger {
        self.trigger.clone().unwrap_or_default()
    }

    /// The agent principal id.
    pub fn agent_id(&self) -> String {
        self.trigger
            .as_ref()
            .and_then(|t| t.agent.as_ref())
            .map(|a| a.id.clone())
            .unwrap_or_default()
    }
}

/// Posts waiting to start one ADOPT_EVALUATE run of a feed trigger (trailing debounce).
#[derive(Clone, PartialEq, Message)]
pub struct FeedPendingRec {
    /// Repo the run works in.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// The agent.
    #[prost(string, tag = "2")]
    pub agent_id: String,
    /// Posts folded in, oldest first.
    #[prost(string, repeated, tag = "3")]
    pub post_ids: Vec<String>,
    /// First post seen (ms).
    #[prost(uint64, tag = "4")]
    pub first_ms: u64,
    /// When the run starts (ms).
    #[prost(uint64, tag = "5")]
    pub due_ms: u64,
}

/// A ref tip seen by the listener.
#[derive(Clone, PartialEq, Message)]
pub struct TipRec {
    /// Target.
    #[prost(message, optional, tag = "1")]
    pub target: Option<ObjectId>,
}

/// A live run token (never returned by any RPC).
#[derive(Clone, PartialEq, Message)]
pub struct RunTokenRec {
    /// Token text.
    #[prost(string, tag = "1")]
    pub token: String,
    /// Token id.
    #[prost(string, tag = "2")]
    pub token_id: String,
}

/// A workspace with its bookkeeping.
#[derive(Clone, PartialEq, Message)]
pub struct WorkspaceRec {
    /// The wire workspace.
    #[prost(message, optional, tag = "1")]
    pub workspace: Option<Workspace>,
    /// Org.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// The parsed devcontainer spec as JSON.
    #[prost(string, tag = "3")]
    pub spec_json: String,
    /// Forwarded ports.
    #[prost(message, repeated, tag = "4")]
    pub ports: Vec<WorkspacePort>,
    /// Last (re)start (ms), for usage.
    #[prost(uint64, tag = "5")]
    pub started_ms: u64,
    /// SHA-256 of the current connection token.
    #[prost(bytes = "vec", tag = "6")]
    pub connection_token_hash: Vec<u8>,
    /// Expiry of the connection token (ms).
    #[prost(uint64, tag = "7")]
    pub token_expires_ms: u64,
    /// Provider handle (a directory for the local provider).
    #[prost(string, tag = "8")]
    pub handle: String,
    /// Machine type to use at the next start.
    #[prost(string, tag = "9")]
    pub next_machine_type: String,
}

impl WorkspaceRec {
    /// The wire workspace.
    pub fn ws(&self) -> Workspace {
        self.workspace.clone().unwrap_or_default()
    }
}

/// A prebuild configuration with scheduling state.
#[derive(Clone, PartialEq, Message)]
pub struct PrebuildConfigRec {
    /// The wire configuration.
    #[prost(message, optional, tag = "1")]
    pub config: Option<WorkspacePrebuildConfig>,
    /// Last scheduled firing (ms).
    #[prost(uint64, tag = "2")]
    pub last_scheduled_ms: u64,
    /// Blob of the devcontainer file at the last prebuild.
    #[prost(message, optional, tag = "3")]
    pub last_spec_blob: Option<ObjectId>,
}

/// A prebuild with the devcontainer blob it used.
#[derive(Clone, PartialEq, Message)]
pub struct PrebuildRec {
    /// The wire prebuild.
    #[prost(message, optional, tag = "1")]
    pub prebuild: Option<WorkspacePrebuild>,
    /// Devcontainer blob.
    #[prost(message, optional, tag = "2")]
    pub spec_blob: Option<ObjectId>,
    /// Machine types it serves.
    #[prost(string, repeated, tag = "3")]
    pub machine_types: Vec<String>,
}

/// One usage line.
#[derive(Clone, PartialEq, Message)]
pub struct UsageRec {
    /// Principal billed.
    #[prost(string, tag = "1")]
    pub principal_id: String,
    /// Machine type.
    #[prost(string, tag = "2")]
    pub machine_type: String,
    /// CPU core seconds.
    #[prost(uint64, tag = "3")]
    pub core_seconds: u64,
    /// Storage byte-hours.
    #[prost(uint64, tag = "4")]
    pub storage_byte_hours: u64,
    /// Cost.
    #[prost(uint64, tag = "5")]
    pub usd_micros: u64,
    /// A prebuild's usage.
    #[prost(bool, tag = "6")]
    pub prebuild: bool,
    /// Workspace or prebuild id.
    #[prost(string, tag = "7")]
    pub subject_id: String,
}

/// Org-level workspace policy.
#[derive(Clone, PartialEq, Message)]
pub struct OrgPolicyRec {
    /// Allowed machine types (empty = all).
    #[prost(string, repeated, tag = "1")]
    pub allowed_machine_types: Vec<String>,
    /// Allowed regions (empty = all).
    #[prost(string, repeated, tag = "2")]
    pub allowed_regions: Vec<String>,
    /// Public ports allowed.
    #[prost(bool, tag = "3")]
    pub allow_public_ports: bool,
    /// Longest idle timeout (0 = no limit).
    #[prost(uint64, tag = "4")]
    pub max_idle_timeout_seconds: u64,
    /// Live workspaces per owner (0 = the deployment default).
    #[prost(uint64, tag = "5")]
    pub max_workspaces_per_owner: u64,
}
