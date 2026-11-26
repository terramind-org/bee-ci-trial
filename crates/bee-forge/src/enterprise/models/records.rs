//! Crate-private stored records of the models module.

use bee_proto::v1::EvalRun;
use prost::Message;

/// An org's model policy, row `policy` of the policy table.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct PolicyRow {
    /// Whether the allow list is in force (false = every model is allowed).
    #[prost(bool, tag = "1")]
    pub restrict: bool,
    /// Model ids the org allows.
    #[prost(string, repeated, tag = "2")]
    pub allowed: Vec<String>,
    /// Monthly budget in micro-USD for gateway-paid inference (0 = none).
    #[prost(uint64, tag = "3")]
    pub budget_usd_micros: u64,
}

/// An eval run with what the executor and the check reporting need beyond `EvalRun`.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct EvalRow {
    /// The public record.
    #[prost(message, optional, tag = "1")]
    pub run: Option<EvalRun>,
    /// Org of the repo.
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Whether to report the result as a check run.
    #[prost(bool, tag = "3")]
    pub report_check: bool,
    /// Check run name.
    #[prost(string, tag = "4")]
    pub check_name: String,
    /// Minimum mean score for the check to pass.
    #[prost(double, tag = "5")]
    pub pass_threshold: f64,
    /// Why reporting the check failed, if it did.
    #[prost(string, tag = "6")]
    pub check_error: String,
}

/// One recorded inference (or batch of identical ones).
#[derive(Clone, PartialEq, Message)]
pub(crate) struct UsageRow {
    /// Model id.
    #[prost(string, tag = "1")]
    pub model: String,
    /// Principal that made the request.
    #[prost(string, tag = "2")]
    pub principal_id: String,
    /// Requests.
    #[prost(uint64, tag = "3")]
    pub requests: u64,
    /// Input tokens.
    #[prost(uint64, tag = "4")]
    pub input_tokens: u64,
    /// Output tokens.
    #[prost(uint64, tag = "5")]
    pub output_tokens: u64,
    /// Cost in micro-USD.
    #[prost(uint64, tag = "6")]
    pub usd_micros: u64,
    /// Whether the org's own provider key paid for it.
    #[prost(bool, tag = "7")]
    pub byok: bool,
    /// Time, ms.
    #[prost(uint64, tag = "8")]
    pub at_ms: u64,
}
