//! Change requests, revisions, reviews, threads, auto-merge and the merge queue
//! (SPEC §11.2–§11.4, PRODUCT §4.13). Serves `ChangeService`.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | the module declaration: surface, tables, migrations, event kinds, audit prefixes |
//! | [`model`] | crate-private row records |
//! | [`vcs`] | what change requests need from the VCS ([`ChangeVcs`], [`Lander`]) and the [`HistoryVcs`] implementation over `bee-history` |
//! | [`brief`] | the brief (SPEC §11.9): pure validation, [`Changes::set_brief`], the brief written with `OpenChange` |
//! | [`text`] | pure helpers: closing keywords, anchor carry-forward, suggestion application |
//! | [`query`] | `ListChanges` filters: the GitHub-compatible query language translated to CR rows |
//! | [`store`] | [`Changes`]: CR lifecycle, revisions, metadata, stacks, templates, comments |
//! | [`review`] | reviews, review requests and auto-assignment, threads, suggestions, viewed files, reactions |
//! | [`landing`] | land decisions, landing through the [`Lander`], auto-merge, merge queue, land conflicts, revert, repo context |
//! | [`service`] | the tonic `ChangeService` ([`service::ChangeServices`]) |
//!
//! # Contracts other modules rely on
//!
//! - `cr.landed` / `cr.closed` carry the [`bee_proto::v1::ChangeRequest`] body including
//!   `change_ids`: [`crate::leases`] releases the CR's leases on them. `cr.landed` also carries
//!   `{"closes_issues": [...]}` in `payload_json` for the issues module.
//! - Land decisions read required checks through [`crate::checks::Checks::check_results`] and
//!   leases through [`crate::leases::Leases::policy_leases`].
//!
//! # Storage
//!
//! Everything lives on the `changes` surface; every state change is one transaction that
//! also records its events and audit entry (`docs/spec/forge-core.md` §2.3).

pub mod brief;
pub mod comments;
pub mod landing;
pub mod model;
pub mod query;
pub mod review;
pub mod service;
pub mod store;
pub mod text;
pub mod vcs;

pub use landing::QueueStep;
pub use store::{ChangeHooks, Changes, MemoryTeams, NewChange, TeamDirectory};
pub use vcs::{ChangeVcs, HistoryVcs, LandFacts, LandIntent, Lander, NoLander, QueueParams};

use crate::module::{EventKind, ForgeModule};
use crate::store::{Index, Migration, MigrationOp, Phase, Surface, TableDef};

/// The changes surface.
pub const SURFACE: Surface = Surface("changes");

/// Change requests: scope repo id, id the zero-padded number, `k1` the CR id (unique),
/// `k2` the current head (hex), `n1` the update time (ms).
pub static CRS: TableDef = TableDef {
    name: "forge_crs",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeK2, Index::ScopeN1],
};

/// Reviews: scope CR id, id a ULID, `k1` the reviewer, `n1` the creation time.
pub static REVIEWS: TableDef = TableDef {
    name: "forge_cr_reviews",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Review threads: scope repo id, id a ULID, `k1` the CR id, `n1` the creation time.
pub static THREADS: TableDef = TableDef {
    name: "forge_cr_threads",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Conversation and review comments: scope CR id, id a ULID, `k1` the thread id (review
/// comments only), `n1` the creation time.
pub static COMMENTS: TableDef = TableDef {
    name: "forge_cr_comments",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Per-reviewer "viewed" files: scope CR id, id `<principal>:<hex path>`, `k1` the principal.
pub static VIEWED: TableDef = TableDef {
    name: "forge_cr_viewed",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Merge-queue entries: scope repo id, id the CR id, `k1` the ref, `n1` the queue order key.
pub static QUEUE: TableDef = TableDef {
    name: "forge_merge_queue",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};

/// Lands out of the merge queue, for throughput: scope `<repo>:<ref>`, `n1` the land time.
pub static QUEUE_LOG: TableDef = TableDef {
    name: "forge_merge_queue_log",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};

/// Land conflicts held for a resolver: scope repo id, `k1` the CR id, `n1` the creation time.
pub static CONFLICTS: TableDef = TableDef {
    name: "forge_land_conflicts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};

/// Comments on changesets outside any CR: scope repo id, `k1` the changeset (hex).
pub static CHANGESET_COMMENTS: TableDef = TableDef {
    name: "forge_changeset_comments",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Counters (`n1`): per-repo CR numbers, merge-queue sequence, round-robin cursors.
pub static COUNTERS: TableDef = TableDef {
    name: "forge_cr_counters",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Conversation comment ids: scope empty, id the comment id, `k1` the CR id, `k2` the repo id
/// (the `CommentService` router finds a CR comment by id alone).
pub static COMMENT_IDS: TableDef = TableDef {
    name: "forge_cr_comment_ids",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Changesets a CR's land put on its target ref (every landed member, and the squash or merge
/// result): scope repo id, id the changeset (hex), `k1` the CR id. Post-land bisect maps a
/// culprit to the CR that landed it through this, even mid-stack.
pub static LANDED_BY: TableDef = TableDef {
    name: "forge_cr_landed",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

static M3: Migration = Migration {
    module: "changes",
    version: 3,
    name: "cr_landed_changesets",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&LANDED_BY)],
};

static M2: Migration = Migration {
    module: "changes",
    version: 2,
    name: "cr_comment_ids",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&COMMENT_IDS)],
};

static M1: Migration = Migration {
    module: "changes",
    version: 1,
    name: "create_changes",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&CRS),
        MigrationOp::CreateTable(&REVIEWS),
        MigrationOp::CreateTable(&THREADS),
        MigrationOp::CreateTable(&COMMENTS),
        MigrationOp::CreateTable(&VIEWED),
        MigrationOp::CreateTable(&QUEUE),
        MigrationOp::CreateTable(&QUEUE_LOG),
        MigrationOp::CreateTable(&CONFLICTS),
        MigrationOp::CreateTable(&CHANGESET_COMMENTS),
        MigrationOp::CreateTable(&COUNTERS),
    ],
};

/// What the changes module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "changes",
    surfaces: &[SURFACE],
    // The posts tables live on this surface so a land, close or reopen updates the change
    // request's post in the same transaction (`crate::posts`).
    migrations: &[
        &M1,
        &M2,
        &M3,
        &crate::posts::M1,
        &crate::posts::M2,
        &crate::swarms::outcomes::MIGRATION,
    ],
    event_kinds: &[
        EventKind::gh(
            "cr.opened",
            "A change request was opened",
            "pull_request",
            "opened",
        ),
        EventKind::gh(
            "cr.revised",
            "A change request gained a revision",
            "pull_request",
            "synchronize",
        ),
        EventKind::gh(
            "cr.landed",
            "A change request landed",
            "pull_request",
            "closed",
        ),
        EventKind::gh(
            "cr.closed",
            "A change request was closed",
            "pull_request",
            "closed",
        ),
        EventKind::gh(
            "cr.reopened",
            "A change request was reopened",
            "pull_request",
            "reopened",
        ),
        EventKind::gh(
            "cr.edited",
            "Title, body or base changed",
            "pull_request",
            "edited",
        ),
        EventKind::gh(
            "cr.ready_for_review",
            "A draft was marked ready",
            "pull_request",
            "ready_for_review",
        ),
        EventKind::gh(
            "cr.converted_to_draft",
            "A change request became a draft",
            "pull_request",
            "converted_to_draft",
        ),
        EventKind::gh(
            "cr.review_requested",
            "Reviewers were requested",
            "pull_request",
            "review_requested",
        ),
        EventKind::gh(
            "cr.review_request_removed",
            "Review requests were removed",
            "pull_request",
            "review_request_removed",
        ),
        EventKind::gh(
            "cr.labels_changed",
            "The labels of a change request changed",
            "pull_request",
            "labeled",
        ),
        EventKind::gh(
            "cr.assignees_changed",
            "The assignees of a change request changed",
            "pull_request",
            "assigned",
        ),
        EventKind::gh(
            "cr.milestone_changed",
            "The milestone of a change request changed",
            "pull_request",
            "milestoned",
        ),
        EventKind::new(
            "cr.issues_linked",
            "Linked issues of a change request changed",
        ),
        EventKind::new("cr.summary_set", "The summary of a change request was set"),
        EventKind::new("cr.brief_set", "The brief of a change request was set"),
        EventKind::gh(
            "cr.auto_merge_enabled",
            "Auto-merge was enabled",
            "pull_request",
            "auto_merge_enabled",
        ),
        EventKind::gh(
            "cr.auto_merge_disabled",
            "Auto-merge was disabled",
            "pull_request",
            "auto_merge_disabled",
        ),
        EventKind::gh(
            "cr.locked",
            "A change request's conversation was locked",
            "pull_request",
            "locked",
        ),
        EventKind::gh(
            "cr.unlocked",
            "A change request's conversation was unlocked",
            "pull_request",
            "unlocked",
        ),
        EventKind::new(
            "cr.reacted",
            "A reaction on a change request or its comments",
        ),
        EventKind::gh(
            "cr_comment.created",
            "A conversation comment was added to a change request",
            "issue_comment",
            "created",
        ),
        EventKind::gh(
            "review.submitted",
            "A review was submitted",
            "pull_request_review",
            "submitted",
        ),
        EventKind::gh(
            "review.dismissed",
            "A review was dismissed",
            "pull_request_review",
            "dismissed",
        ),
        EventKind::gh(
            "review_comment.created",
            "A line or file comment was added",
            "pull_request_review_comment",
            "created",
        ),
        EventKind::new("review_thread.created", "A review thread was started"),
        EventKind::gh(
            "review_thread.resolved",
            "A review thread was resolved",
            "pull_request_review_thread",
            "resolved",
        ),
        EventKind::gh(
            "review_thread.unresolved",
            "A review thread was unresolved",
            "pull_request_review_thread",
            "unresolved",
        ),
        EventKind::new(
            "review_thread.outdated",
            "The lines a review thread anchors to changed",
        ),
        EventKind::gh(
            "changeset_comment.created",
            "A comment was added to a changeset",
            "commit_comment",
            "created",
        ),
        EventKind::gh(
            "merge_queue.entered",
            "A change request entered the merge queue",
            "merge_group",
            "checks_requested",
        ),
        EventKind::new("merge_queue.updated", "A merge-queue entry changed state"),
        EventKind::gh(
            "merge_queue.checks_requested",
            "Checks were requested on the combined result of a merge-queue entry",
            "merge_group",
            "checks_requested",
        ),
        EventKind::new("merge_queue.left", "A change request left the merge queue"),
        EventKind::new(
            "land_conflict.opened",
            "A land hit a conflict for the resolver",
        ),
        EventKind::new(
            "land_conflict.resolved",
            "A resolver submitted a resolution",
        ),
    ],
    audit_prefixes: &[
        "cr.",
        "review.",
        "merge_queue.",
        "land_conflict.",
        "changeset_comment.",
    ],
};
