//! Swarm records, task attempts and admission over the existing agent runs.
//!
//! Membership and the run queue share the agents surface. An accepted join queues its run
//! in the same transaction that admits it, and stop cancels every run in one transaction.

mod model;
pub(crate) mod outcomes;
pub mod service;
pub(crate) mod store;

use crate::agents::SURFACE;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Migration, MigrationOp, Phase, TableDef};

static SWARMS: TableDef = TableDef {
    name: "forge_swarms",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

// A run belongs to at most one swarm. Scoped by repository, with the swarm id in k1.
static MEMBERS: TableDef = TableDef {
    name: "forge_swarm_runs",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

pub(crate) static MIGRATION: Migration = Migration {
    module: "agents",
    version: 4,
    name: "create_swarms",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&SWARMS),
        MigrationOp::CreateTable(&MEMBERS),
    ],
};

/// Swarm event kinds and audit prefix.
pub static MODULE: ForgeModule = ForgeModule {
    name: "swarms",
    surfaces: &[],
    migrations: &[],
    event_kinds: &[
        EventKind::new("swarm.created", "A swarm was created"),
        EventKind::new("swarm.state_changed", "A swarm changed state"),
        EventKind::new("swarm.status_changed", "The lead updated the swarm status"),
        EventKind::new("swarm.task_added", "A task was added"),
        EventKind::new("swarm.task_started", "A task attempt started"),
        EventKind::new("swarm.task_finished", "A task attempt ended"),
        EventKind::new("swarm.join_requested", "An agent was offered to the swarm"),
        EventKind::new("swarm.join_decided", "A join request was decided"),
    ],
    audit_prefixes: &["swarm."],
};
