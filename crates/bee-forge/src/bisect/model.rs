//! Row records of bisect sessions (protobuf, additive-only like the wire contract), stored in
//! `ci::BISECTS`.

use bee_proto::v1::Bisect;
use prost::Message;

/// One probe or manual mark of a candidate.
#[derive(Clone, PartialEq, Message)]
pub struct ProbeRecord {
    /// Candidate index (0 = oldest).
    #[prost(uint32, tag = "1")]
    pub index: u32,
    /// Candidate changeset (hex).
    #[prost(string, tag = "2")]
    pub changeset: String,
    /// The prober's handle (a workflow run id for CI probes); empty for marks and history.
    #[prost(string, tag = "3")]
    pub probe_id: String,
    /// `pass`, `fail`, `skip`, or empty while running.
    #[prost(string, tag = "4")]
    pub result: String,
    /// Round the probe belongs to (0 = known from history or a manual mark).
    #[prost(uint32, tag = "5")]
    pub round: u32,
    /// Who marked it (manual sessions), `history` for check runs already reported, `probe` for
    /// a bisect probe, or `confirm` for the re-run of the failing changeset.
    #[prost(string, tag = "6")]
    pub source: String,
}

/// A durable claim for a probe that is about to be started.
#[derive(Clone, PartialEq, Message)]
pub struct ProbeIntent {
    /// Candidate index (0 = oldest).
    #[prost(uint32, tag = "1")]
    pub index: u32,
    /// Round the probe belongs to (0 for the confirmation probe).
    #[prost(uint32, tag = "2")]
    pub round: u32,
    /// `probe` or `confirm`.
    #[prost(string, tag = "3")]
    pub source: String,
    /// Store clock timestamp when this slot was claimed.
    #[prost(int64, tag = "4")]
    pub started_at_ms: i64,
}

/// A bisect session.
#[derive(Clone, PartialEq, Message)]
pub struct BisectRecord {
    /// The public view.
    #[prost(message, optional, tag = "1")]
    pub bisect: Option<Bisect>,
    /// Candidates (hex), oldest first; the last is the failing changeset.
    #[prost(string, repeated, tag = "2")]
    pub candidates: Vec<String>,
    /// Probes and marks, in the order they were made.
    #[prost(message, repeated, tag = "3")]
    pub probes: Vec<ProbeRecord>,
    /// Rounds launched so far.
    #[prost(uint32, tag = "4")]
    pub rounds: u32,
    /// Workflow the failing check came from (empty for external checks).
    #[prost(string, tag = "5")]
    pub workflow_path: String,
    /// Job key in that workflow.
    #[prost(string, tag = "6")]
    pub job_key: String,
    /// The failing check run that started the session.
    #[prost(string, tag = "7")]
    pub failing_check_run_id: String,
    /// The failing workflow run (CI checks).
    #[prost(string, tag = "8")]
    pub failing_run_id: String,
    /// Where the failure can be seen.
    #[prost(string, tag = "9")]
    pub failing_url: String,
    /// `auto` (probes through a prober) or `manual` (good/bad marks).
    #[prost(string, tag = "10")]
    pub mode: String,
    /// Open a revert CR when the culprit is found.
    #[prost(bool, tag = "11")]
    pub open_revert: bool,
    /// Probes per round.
    #[prost(uint32, tag = "12")]
    pub parallelism: u32,
    /// Why the session ended as it did, and what happened to the revert.
    #[prost(string, tag = "13")]
    pub note: String,
    /// The CR that landed the culprit.
    #[prost(string, tag = "14")]
    pub culprit_change_request_id: String,
    /// Its number.
    #[prost(uint64, tag = "15")]
    pub culprit_change_request_number: u64,
    /// Its author (principal id).
    #[prost(string, tag = "16")]
    pub culprit_author: String,
    /// The revert landed.
    #[prost(bool, tag = "17")]
    pub revert_landed: bool,
    /// Who started the session.
    #[prost(string, tag = "18")]
    pub started_by: String,
    /// The repo's org.
    #[prost(string, tag = "19")]
    pub org_id: String,
    /// Re-run the failing check once at the failing changeset before blaming anyone; a pass
    /// ends the session as flaky (automatic sessions).
    #[prost(bool, tag = "20")]
    pub confirm_tip: bool,
    /// Probe slots claimed before calling the external prober. Absent in older records.
    #[prost(message, repeated, tag = "21")]
    pub starting_probes: Vec<ProbeIntent>,
}
