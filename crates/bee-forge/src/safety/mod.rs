//! Beehive's asked-against-done record, independent reviews and human intervention.

/// Interruption and merge freeze shared with Bee's publication paths.
pub mod control;
mod detectors;
mod evidence;
mod listener;
/// The SafetyService over Beehive's work and event stores.
pub mod service;
mod store;

pub(crate) use evidence::evidence_event;
pub use evidence::{record_changeset, record_landing_attempt};
pub(crate) use listener::SafetyListener;
pub(crate) use store::unacknowledged_swarm_pause;

use crate::agents::SURFACE;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Index, Migration, MigrationOp, Phase, TableDef};

static RECORDS: TableDef = TableDef {
    name: "forge_safety_records",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};

// Scoped to the repository. k1 is the only recipient who can see or acknowledge the item.
static ATTENTION: TableDef = TableDef {
    name: "forge_safety_attention",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};

pub(crate) static MIGRATION: Migration = Migration {
    module: "agents",
    version: 5,
    name: "create_safety_record",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&RECORDS),
        MigrationOp::CreateTable(&ATTENTION),
        MigrationOp::CreateTable(&control::LAND_FREEZE),
        MigrationOp::CreateTable(&evidence::LAND_ATTEMPTS),
    ],
};

/// The safety event and audit namespace. Its tables share the agents transaction.
pub static MODULE: ForgeModule = ForgeModule {
    name: "safety",
    surfaces: &[],
    migrations: &[],
    event_kinds: &[
        EventKind::new("safety.record_updated", "The work evidence was recorded"),
        EventKind::new(
            "safety.evidence_observed",
            "An attributed run produced new evidence",
        ),
        EventKind::new("safety.finding_reported", "A safety finding was recorded"),
        EventKind::new("safety.finding_resolved", "A person resolved a finding"),
        EventKind::new("safety.verdict_set", "A person recorded a verdict"),
        EventKind::new(
            "safety.review_requested",
            "Work is ready for an independent review",
        ),
        EventKind::new(
            "safety.review_submitted",
            "A reviewer submitted findings and a proposal",
        ),
        EventKind::new(
            "safety.concern_raised",
            "An agent raised a concern about its task",
        ),
        EventKind::new("safety.tripwire", "A severe finding interrupted work"),
        EventKind::new(
            "safety.attention_acknowledged",
            "The answerable person acknowledged an item",
        ),
        EventKind::new("safety.stopped", "An organization owner stopped agent work"),
        EventKind::new(
            "safety.resumed",
            "An organization owner lifted the agent merge freeze",
        ),
    ],
    audit_prefixes: &["safety."],
};
