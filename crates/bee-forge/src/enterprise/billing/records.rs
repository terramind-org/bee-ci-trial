//! Crate-private stored records (protobuf-encoded row bodies) of rows that have no
//! `bee.v1` message.

use bee_proto::v1::BillingCycle;
use prost::Message;

/// An account's plan subscription, row `plan` of the accounts table.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct AccountRow {
    /// Current plan id.
    #[prost(string, tag = "1")]
    pub plan_id: String,
    /// Billing cycle.
    #[prost(enumeration = "BillingCycle", tag = "2")]
    pub cycle: i32,
    /// Seats purchased.
    #[prost(uint32, tag = "3")]
    pub seats_purchased: u32,
    /// Start of the current term (first millisecond of a UTC month).
    #[prost(uint64, tag = "4")]
    pub term_start_ms: u64,
    /// Plan scheduled for the end of the term.
    #[prost(string, tag = "5")]
    pub pending_plan_id: String,
    /// Seats scheduled for the end of the term (0 = none).
    #[prost(uint32, tag = "6")]
    pub pending_seats: u32,
    /// Last change, ms.
    #[prost(uint64, tag = "7")]
    pub updated_ms: u64,
}

/// One metered usage event, scoped by the org that incurred it.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct UsageRow {
    /// Repo id, or empty.
    #[prost(string, tag = "1")]
    pub repo_id: String,
    /// The principal that did the work.
    #[prost(string, tag = "2")]
    pub principal_id: String,
    /// The principal billed for it: the owner of an agent, else the principal itself.
    #[prost(string, tag = "3")]
    pub attributed_to: String,
    /// `BillingProduct`.
    #[prost(int32, tag = "4")]
    pub product: i32,
    /// SKU.
    #[prost(string, tag = "5")]
    pub sku: String,
    /// Quantity in millionths of a unit.
    #[prost(uint64, tag = "6")]
    pub quantity_micro: u64,
    /// Unit.
    #[prost(string, tag = "7")]
    pub unit: String,
    /// Time of the usage, ms.
    #[prost(uint64, tag = "8")]
    pub at_ms: u64,
    /// Idempotency key, or empty.
    #[prost(string, tag = "9")]
    pub key: String,
}

/// A fired alert marker.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct AlertRow {
    /// When it fired, ms.
    #[prost(uint64, tag = "1")]
    pub fired_ms: u64,
    /// Spend at that time, micro-USD.
    #[prost(uint64, tag = "2")]
    pub spent_micro_usd: u64,
}
