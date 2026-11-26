//! Tables and stored records of the moderation surface.
//!
//! Every table uses the shared row shape ([`crate::store`]): `scope` partitions, `id` names the
//! row inside the scope, `k1`/`k2` are secondary keys, `n1` is an expiry, a time or a counter.
//! Rows whose body is a proto message from `moderation.proto` store that message as is; the
//! few records the proto does not model are the small messages declared here.
//!
//! | table | scope | id | `n1` | body |
//! |---|---|---|---|---|
//! | [`BLOCKS`] | `org:<org>` or `user:<user>` | blocked principal | expiry (none = permanent) | `BlockEntry` |
//! | [`LIMITS`] | `repo:<id>`, `org:<id>` or `user:<id>` | `limit` | expiry | `InteractionLimit` |
//! | [`REPORTS`] | `all` | report ULID | | `ModerationReport`; `k1` = queue, `k2` = org queue or `site` |
//! | [`COMMENTS`] | `c` | comment id | | [`CommentState`] |
//! | [`EDITS`] | comment id | edit ULID | | `CommentEdit` |
//! | [`LOCKS`] | repo id | `<kind>:<subject id>` | | `ModerationLock` |
//! | [`MODERATORS`] | org id | `p:<principal>` or `t:<team>` | | `ModerationModerator` |
//! | [`FIRST_TIME`] | `repo:<id>` or `org:<id>` | `s` | | `ModerationFirstTimeContributorSettings` |
//! | [`FLOOD`] | repo id | `limits` | | `ModerationFloodLimits` |
//! | [`THROTTLES`] | repo id | throttled principal | expiry | `ModerationThrottledPrincipal` |
//! | [`ACTIVITY`] | `<repo>:<owner>:<kind>` | ULID | admission time | none; `k1` = actor |
//! | [`COUNTERS`] | repo id | `cr:p:<principal>`, `cr:o:<owner>`, `adm:<owner>:<kind>` | the count | none |
//! | [`REPUTATION`] | `rep` | principal | | [`Reputation`] |
//! | [`TAKEDOWNS`] | `all` | takedown ULID | | `Takedown`; `k1` = subject key, `k2` = `repo:<id>` |
//! | [`MARKS`] | `mark` | `repo:<id>` or `comment:<id>` | | none; `k1` = active takedown id |

use bee_proto::v1::{Principal, SubjectRef};
use prost::Message;

use super::SURFACE;
use crate::store::{Index, Migration, MigrationOp, Phase, TableDef};

macro_rules! table {
    ($(#[$m:meta])* $name:ident, $phys:literal, $indexes:expr) => {
        $(#[$m])*
        pub static $name: TableDef = TableDef {
            name: $phys,
            surface: SURFACE,
            unique_k1: false,
            unique_k2: false,
            indexes: $indexes,
        };
    };
}

table!(
    /// Blocks.
    BLOCKS,
    "forge_moderation_blocks",
    &[Index::N1]
);
table!(
    /// Interaction limits.
    LIMITS,
    "forge_moderation_limits",
    &[Index::N1]
);
table!(
    /// Content reports.
    REPORTS,
    "forge_moderation_reports",
    &[Index::ScopeK1, Index::ScopeK2]
);
table!(
    /// Per-comment moderation state (minimized reason, edited flag).
    COMMENTS,
    "forge_moderation_comments",
    &[]
);
table!(
    /// Per-comment edit history.
    EDITS,
    "forge_moderation_comment_edits",
    &[]
);
table!(
    /// Conversation locks.
    LOCKS,
    "forge_moderation_locks",
    &[]
);
table!(
    /// Moderators (principals and teams).
    MODERATORS,
    "forge_moderation_moderators",
    &[Index::ScopeK1]
);
table!(
    /// First-time contributor settings.
    FIRST_TIME,
    "forge_moderation_first_time",
    &[]
);
table!(
    /// Flood limits per repo.
    FLOOD,
    "forge_moderation_flood_limits",
    &[]
);
table!(
    /// Throttled principals per repo.
    THROTTLES,
    "forge_moderation_throttles",
    &[Index::N1]
);
table!(
    /// Admitted writes, the sliding window of the creation rate limits.
    ACTIVITY,
    "forge_moderation_activity",
    &[Index::ScopeN1, Index::N1, Index::ScopeK1]
);
table!(
    /// Counters (open CRs, admission locks).
    COUNTERS,
    "forge_moderation_counters",
    &[]
);
table!(
    /// Principal reputation tallies.
    REPUTATION,
    "forge_moderation_reputation",
    &[]
);
table!(
    /// Takedown notices.
    TAKEDOWNS,
    "forge_moderation_takedowns",
    &[Index::ScopeK1, Index::ScopeK2]
);
table!(
    /// Active takedown markers, one point read per subject on the write path.
    MARKS,
    "forge_moderation_takedown_marks",
    &[]
);

pub(super) static M1: Migration = Migration {
    module: "moderation",
    version: 1,
    name: "create_moderation",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&BLOCKS),
        MigrationOp::CreateTable(&LIMITS),
        MigrationOp::CreateTable(&REPORTS),
        MigrationOp::CreateTable(&COMMENTS),
        MigrationOp::CreateTable(&EDITS),
        MigrationOp::CreateTable(&LOCKS),
        MigrationOp::CreateTable(&MODERATORS),
        MigrationOp::CreateTable(&FIRST_TIME),
        MigrationOp::CreateTable(&FLOOD),
        MigrationOp::CreateTable(&THROTTLES),
        MigrationOp::CreateTable(&ACTIVITY),
        MigrationOp::CreateTable(&COUNTERS),
        MigrationOp::CreateTable(&REPUTATION),
        MigrationOp::CreateTable(&TAKEDOWNS),
        MigrationOp::CreateTable(&MARKS),
    ],
};

/// Moderation state of one comment. The comment itself belongs to the work module; this keeps
/// what moderation knows about it.
#[derive(Clone, PartialEq, Message)]
pub struct CommentState {
    /// Comment id.
    #[prost(string, tag = "1")]
    pub comment_id: String,
    /// What the comment is on.
    #[prost(message, optional, tag = "2")]
    pub subject: Option<SubjectRef>,
    /// `MinimizedReason` value; 0 = shown.
    #[prost(int32, tag = "3")]
    pub reason: i32,
    /// Who minimized it.
    #[prost(message, optional, tag = "4")]
    pub by: Option<Principal>,
    /// When it was minimized (ms).
    #[prost(uint64, tag = "5")]
    pub at_ms: u64,
    /// Whether the comment has an edit history.
    #[prost(bool, tag = "6")]
    pub edited: bool,
}

/// Tally behind a principal's reputation score.
#[derive(Clone, PartialEq, Message)]
pub struct Reputation {
    /// Contributions that landed or were accepted.
    #[prost(uint64, tag = "1")]
    pub landed: u64,
    /// Contributions that were rejected.
    #[prost(uint64, tag = "2")]
    pub rejected: u64,
    /// Contributions reported and confirmed as spam.
    #[prost(uint64, tag = "3")]
    pub spam: u64,
}
