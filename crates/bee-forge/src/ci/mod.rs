//! Workflows, runs, jobs, runners, logs, artifacts, caches, secrets, environments and
//! deployments (PRODUCT §4.14, SPEC §11.6, `docs/spec/workflows.md`). Serves
//! `WorkflowService` and `RunnerService`.
//!
//! The workflow language itself (parsing, triggers, expressions, matrices, the job graph,
//! concurrency decisions and plans) is the pure layer-1 crate `bee-workflow`. This module is
//! everything around it that has state: which workflows a repo has, the runs an event starts,
//! the job queue and the runners that drain it, logs, artifacts, caches, secrets, environments
//! and the check runs that make CI results visible to land policy.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | the module declaration: surface, tables, migrations, event kinds, audit prefixes |
//! | [`model`] | crate-private row records |
//! | [`ports`] | what CI needs from outside: blob storage ([`CiBlobs`]), a signing key ([`KeyPort`]), the secret cipher, URL signing, settings ([`CiConfig`]) |
//! | [`vcs`] | what CI reads from the VCS ([`CiVcs`]) and [`HistoryCiVcs`] over `bee-history` |
//! | [`convert`] | events, plans and permissions translated between `bee-workflow` and storage |
//! | [`engine`] | [`Ci`]: run creation, the run/job state machine, the job graph, concurrency, cancellation, re-runs, check runs, the maintenance tick |
//! | [`trigger`] | forge events → workflow events ([`CiListener`]), schedules, dispatch |
//! | [`runners`] | runners, registration tokens, groups, job acquisition with leases and heartbeats, machine classes and images |
//! | [`logs`] | chunked logs in the blob store, masking, tailing, search, annotations, archives |
//! | [`artifacts`] | artifacts and caches |
//! | [`sealed`] | sealed boxes (libsodium `crypto_box_seal`) and the X25519 key port ([`sealed::SealedBoxPort`]) behind `SecretService` |
//! | [`secrets`] | secrets (encrypted at rest) and variables at org, repo and environment scope |
//! | [`environments`] | environments, protection rules, deployments and reviews |
//! | [`deployments`] | API deployments, deployment statuses, custom protection rules, green status |
//! | [`green`] | `green/<ref>`: moved forward when `post_land` checks pass ([`green::GreenListener`]) |
//! | [`vault_service`] | the tonic `SecretService` |
//! | [`transfer`] | the signed `/_ci/...` transfer URLs (artifacts, caches, logs, one-time secrets), HTTP-stack independent |
//! | [`deploy_service`] | the tonic `DeploymentService` |
//! | [`policy`] | actions policies, required workflows, OIDC, attestations, usage, badges |
//! | [`service`] | the tonic `WorkflowService` and `RunnerService` ([`CiServices`]) |
//! | [`runner`] | the local runner library that executes jobs in a temp workspace |
//! | [`util`] | encodings and archives shared by the server and the runner |
//!
//! # Storage
//!
//! Everything lives on the `ci` surface; every state change is one transaction that also
//! records its events and audit entry (`docs/spec/forge-core.md` §2.3). Check runs live on the
//! `checks` surface and are reconciled after each change ([`Ci::sync_checks`]), so a checks
//! outage never stops CI and its results arrive when checks recover.

pub mod artifacts;
pub mod convert;
pub mod deploy_service;
pub mod deployments;
pub mod engine;
pub mod environments;
pub mod green;
pub mod logs;
pub mod model;
pub mod policy;
pub mod ports;
pub mod runner;
pub mod runner_service;
pub mod runners;
pub mod sealed;
pub mod secrets;
pub mod service;
pub mod transfer;
pub mod trigger;
pub mod util;
pub mod vault_service;
pub mod vcs;

pub use engine::{Ci, CiHooks, ForkRunGate, TriggerContext};
pub use ports::{
    CiBlobs, CiConfig, HmacKey, KeyPort, MemoryCiBlobs, SecretCipher, StoreCiBlobs, UrlSigner,
};
pub use service::CiServices;
pub use trigger::CiListener;
pub use vcs::{CiVcs, HistoryCiVcs, TreeFile};

use crate::module::{EventKind, ForgeModule};
use crate::store::{Index, Migration, MigrationOp, Phase, Surface, TableDef};

/// The ci surface.
pub const SURFACE: Surface = Surface("ci");

const fn table(
    name: &'static str,
    unique_k1: bool,
    unique_k2: bool,
    indexes: &'static [Index],
) -> TableDef {
    TableDef {
        name,
        surface: SURFACE,
        unique_k1,
        unique_k2,
        indexes,
    }
}

/// Workflows seen in a repo: scope repo id, id a ULID, `k1` the path (unique), `k2` `schedule`
/// when the default-ref version has cron triggers.
pub static WORKFLOWS: TableDef = table(
    "forge_ci_workflows",
    true,
    false,
    &[Index::ScopeK1, Index::K2],
);

/// Runs (latest attempt): scope repo id, id a ULID, `k1` the workflow id, `k2` the head
/// changeset (hex), `n1` the creation time.
pub static RUNS: TableDef = table(
    "forge_ci_runs",
    false,
    false,
    &[Index::ScopeK1, Index::ScopeK2, Index::ScopeN1],
);

/// Earlier attempts of a run: scope run id, id the zero-padded attempt number.
pub static ATTEMPTS: TableDef = table("forge_ci_attempts", false, false, &[]);

/// Jobs: scope repo id, id a ULID, `k1` the run id, `n1` the creation time.
pub static JOBS: TableDef = table("forge_ci_jobs", false, false, &[Index::ScopeK1]);

/// Jobs waiting for a runner: scope `""`, id the job id, `k1` the repo id, `n1` the queue time.
pub static QUEUE: TableDef = table("forge_ci_queue", false, false, &[Index::N1]);

/// Every unfinished job by id: scope `""`, id the job id, `n1` the next time the tick must
/// look at it (lease expiry, timeout, wait timer).
pub static JOB_INDEX: TableDef = table("forge_ci_job_index", false, false, &[Index::N1]);

/// Log chunks: scope job id, id the zero-padded byte offset, `n1` the offset.
pub static LOGS: TableDef = table("forge_ci_logs", false, false, &[Index::ScopeN1]);

/// Runners: scope `""`, id a ULID, `k1` `<owner>/<name>` (unique), `k2` the owner key
/// (`org:<id>` or `repo:<id>`).
pub static RUNNERS: TableDef = table("forge_ci_runners", true, false, &[Index::K2]);

/// Runner groups: scope org id, id a ULID, `k1` the lower-case name (unique).
pub static GROUPS: TableDef = table("forge_ci_runner_groups", true, false, &[Index::ScopeK1]);

/// Registration and removal tokens: scope `""`, id the SHA-256 of the token, `n1` the expiry.
pub static TOKENS: TableDef = table("forge_ci_runner_tokens", false, false, &[Index::N1]);

/// Custom runner images: scope org id, id a ULID, `k1` the name (unique).
pub static IMAGES: TableDef = table("forge_ci_runner_images", true, false, &[]);

/// Artifacts: scope repo id, id a ULID, `k1` the run id, `k2` the name, `n1` the expiry.
pub static ARTIFACTS: TableDef = table(
    "forge_ci_artifacts",
    false,
    false,
    &[Index::ScopeK1, Index::N1],
);

/// Caches: scope repo id, id a ULID, `k1` the ref, `k2` the key, `n1` the last access.
pub static CACHES: TableDef = table(
    "forge_ci_caches",
    false,
    false,
    &[Index::ScopeK1, Index::ScopeK2, Index::ScopeN1],
);

/// Secrets: scope the secret scope key, id the name.
pub static SECRETS: TableDef = table("forge_ci_secrets", false, false, &[]);

/// Variables: scope the variable scope key, id the name.
pub static VARIABLES: TableDef = table("forge_ci_variables", false, false, &[]);

/// Environments: scope repo id, id the lower-case name.
pub static ENVIRONMENTS: TableDef = table("forge_ci_environments", false, false, &[]);

/// Deployments: scope repo id, id a ULID, `k1` the lower-case environment, `k2` the run id,
/// `n1` the creation time.
pub static DEPLOYMENTS: TableDef = table(
    "forge_ci_deployments",
    false,
    false,
    &[Index::ScopeK1, Index::ScopeK2],
);

/// Actions policies and OIDC templates: scope `""`, id `org:<id>` or `repo:<id>`.
pub static POLICIES: TableDef = table("forge_ci_policies", false, false, &[]);

/// Required workflows: scope org id, id a ULID.
pub static REQUIRED: TableDef = table("forge_ci_required", false, false, &[]);

/// Attestations: scope repo id, id a ULID, `k1` the first subject digest, `k2` the org id.
pub static ATTESTATIONS: TableDef = table(
    "forge_ci_attestations",
    false,
    false,
    &[Index::ScopeK1, Index::K2],
);

/// Run approvals and environment reviews: scope run id, id a ULID.
pub static APPROVALS: TableDef = table("forge_ci_approvals", false, false, &[]);

/// Concurrency groups: scope repo id, id the lower-case group name.
pub static CONCURRENCY: TableDef = table("forge_ci_concurrency", false, false, &[]);

/// Last seen tip per ref: scope repo id, id the ref name.
pub static REF_TIPS: TableDef = table("forge_ci_ref_tips", false, false, &[]);

/// Metered job time: scope repo id, id the job id, `k1` the workflow id, `n1` the completion
/// time.
pub static USAGE: TableDef = table("forge_ci_usage", false, false, &[Index::ScopeK1]);

/// Counters (`n1`): run numbers per workflow, numeric run ids, schedule minutes.
pub static COUNTERS: TableDef = table("forge_ci_counters", false, false, &[]);

/// Bisect sessions (`crate::bisect`): scope repo id, id a ULID, `k1` the state (`running`,
/// `found`, `inconclusive`, `cancelled`), `k2` the ref, `n1` the start time.
pub static BISECTS: TableDef = table(
    "forge_ci_bisects",
    false,
    false,
    &[Index::ScopeK1, Index::K1, Index::ScopeK2, Index::ScopeN1],
);

static M1: Migration = Migration {
    module: "ci",
    version: 1,
    name: "create_ci",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&WORKFLOWS),
        MigrationOp::CreateTable(&RUNS),
        MigrationOp::CreateTable(&ATTEMPTS),
        MigrationOp::CreateTable(&JOBS),
        MigrationOp::CreateTable(&QUEUE),
        MigrationOp::CreateTable(&JOB_INDEX),
        MigrationOp::CreateTable(&LOGS),
        MigrationOp::CreateTable(&RUNNERS),
        MigrationOp::CreateTable(&GROUPS),
        MigrationOp::CreateTable(&TOKENS),
        MigrationOp::CreateTable(&IMAGES),
        MigrationOp::CreateTable(&ARTIFACTS),
        MigrationOp::CreateTable(&CACHES),
        MigrationOp::CreateTable(&SECRETS),
        MigrationOp::CreateTable(&VARIABLES),
        MigrationOp::CreateTable(&ENVIRONMENTS),
        MigrationOp::CreateTable(&DEPLOYMENTS),
        MigrationOp::CreateTable(&POLICIES),
        MigrationOp::CreateTable(&REQUIRED),
        MigrationOp::CreateTable(&ATTESTATIONS),
        MigrationOp::CreateTable(&APPROVALS),
        MigrationOp::CreateTable(&CONCURRENCY),
        MigrationOp::CreateTable(&REF_TIPS),
        MigrationOp::CreateTable(&USAGE),
        MigrationOp::CreateTable(&COUNTERS),
    ],
};

static M2: Migration = Migration {
    module: "ci",
    version: 2,
    name: "create_ci_bisects",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&BISECTS)],
};

/// What the ci module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "ci",
    surfaces: &[SURFACE],
    migrations: &[&M1, &M2],
    event_kinds: &[
        EventKind::gh(
            "workflow_run.requested",
            "A workflow run was queued",
            "workflow_run",
            "requested",
        ),
        EventKind::gh(
            "workflow_run.in_progress",
            "A workflow run started",
            "workflow_run",
            "in_progress",
        ),
        EventKind::gh(
            "workflow_run.completed",
            "A workflow run finished",
            "workflow_run",
            "completed",
        ),
        EventKind::gh(
            "workflow_job.queued",
            "A job was queued",
            "workflow_job",
            "queued",
        ),
        EventKind::gh(
            "workflow_job.waiting",
            "A job is waiting on environment protection",
            "workflow_job",
            "waiting",
        ),
        EventKind::gh(
            "workflow_job.in_progress",
            "A runner picked up a job",
            "workflow_job",
            "in_progress",
        ),
        EventKind::gh(
            "workflow_job.completed",
            "A job finished",
            "workflow_job",
            "completed",
        ),
        EventKind::new(
            "workflow_usage.recorded",
            "Runner time of a finished job was metered",
        ),
        EventKind::new(
            "workflow.updated",
            "A workflow was enabled, disabled, added or removed",
        ),
        EventKind::gh(
            "deployment.created",
            "A deployment was created",
            "deployment",
            "created",
        ),
        EventKind::gh(
            "deployment_status.created",
            "A deployment changed state",
            "deployment_status",
            "created",
        ),
        EventKind::gh(
            "deployment_review.requested",
            "A deployment waits for a required reviewer",
            "deployment_review",
            "requested",
        ),
        EventKind::gh(
            "deployment_review.decided",
            "A reviewer approved or rejected a deployment",
            "deployment_review",
            "approved",
        ),
        EventKind::gh(
            "repository_dispatch.created",
            "A repository_dispatch event was fired",
            "repository_dispatch",
            "created",
        ),
        EventKind::new("runner.registered", "A runner registered"),
        EventKind::new("runner.removed", "A runner was removed"),
        EventKind::new("bisect.started", "A post-land bisect started"),
        EventKind::new("bisect.found", "A bisect found the first failing changeset"),
        EventKind::new(
            "bisect.finished",
            "A bisect ended without a culprit (inconclusive or cancelled)",
        ),
        EventKind::new(
            "bisect.reverted",
            "A bisect opened (or landed) the revert of the first failing change",
        ),
        EventKind::new(
            "bisect.flaky",
            "A post-land failure passed when re-run at the failing changeset; nothing is blamed",
        ),
    ],
    audit_prefixes: &[
        "workflow.",
        "runner.",
        "deployment.",
        "bisect.",
        "secret.",
        "variable.",
        "environment.",
        "artifact.",
        "cache.",
        "actions_policy.",
        "attestation.",
    ],
};
