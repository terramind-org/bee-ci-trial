//! Agents and workspaces: the agent registry, agent runs and sessions, instructions, memory,
//! context bundles, component watchers, automatic agent review and cloud workspaces
//! (PRODUCT §4.19). Serves `AgentService` and `WorkspaceService`; the design is in
//! `docs/spec/agents.md`.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | surfaces, tables, migrations, event kinds, [`Agents`], wiring and workers |
//! | [`model`] | crate-private stored records |
//! | [`ports`] | what the server backs: [`AgentRunner`], [`RunTokenIssuer`], [`AgentBlobs`], [`AgentRepo`], [`WorkspaceProvider`] |
//! | [`registry`] | runtimes, agent definitions, owner budgets, principal summaries, dashboards, work items, the activity stream |
//! | [`runs`] | runs: start, queue, dispatch through the runner, events, cancel, retry, sessions |
//! | [`triggers`] | the event listener: issue assignment, @mentions, review config, CR outcomes, watchers with debounce, schedules |
//! | [`feed`] | feed triggers: agents follow repos and evaluate their published posts (ADOPT_EVALUATE runs) |
//! | [`context`] | repo instructions, memory, context bundles |
//! | [`watchers`] | component watchers, the anomaly feed, review configuration |
//! | [`workspaces`] | cloud workspaces, prebuilds, ports, dotfiles, egress policy, usage |
//! | [`devcontainer`] | the `devcontainer.json` parser |
//! | [`cron`] | five-field cron schedules |
//! | [`local`] | [`local::LocalProvider`]: workspaces as working-copy directories |
//! | [`fake`] | [`fake::FakeRunner`]: a deterministic runner for tests and `--dev` |
//! | [`service`] | the tonic `AgentService` and `WorkspaceService` |
//!
//! # Storage
//!
//! Two surfaces (failure domains): `agents` (registry, runs, sessions, memory, bundles,
//! watchers, anomalies) and `workspaces`. Every state change is one transaction that records
//! its events and audit entry (`docs/spec/forge-core.md` §2.3).

pub mod context;
pub mod cron;
pub mod devcontainer;
pub mod fake;
pub mod feed;
pub mod local;
pub mod model;
pub mod ports;
pub mod registry;
pub mod runs;
pub mod service;
pub mod triggers;
pub mod watchers;
pub mod workspaces;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

pub use ports::{
    AgentBlobs, AgentRepo, AgentRunner, KeyRingIssuer, MemoryBlobs, RepoFile, RunGrant, RunJob,
    RunOutcome, RunReporter, RunTokenIssuer, RunnerEvent, WorkspaceProvider,
};
pub use registry::{ActivityFilter, AgentDefinition, DefinitionPatch, OwnerBudget, Schedule};
pub use runs::{StartRun, Trigger};
pub use workspaces::{MachineType, OrgWorkspacePolicy, WorkspaceConfig, Workspaces};

use crate::changes::Changes;
use crate::cx::Cx;
use crate::error::Result;
use crate::forge::Forge;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Db, Index, Migration, MigrationOp, Phase, Surface, TableDef};

/// The agents surface.
pub const SURFACE: Surface = Surface("agents");
/// The workspaces surface.
pub const WS_SURFACE: Surface = Surface("workspaces");

macro_rules! table {
    ($(#[$m:meta])* $name:ident, $t:literal, $s:expr, $u1:expr, $u2:expr, [$($i:expr),*]) => {
        $(#[$m])*
        pub static $name: TableDef = TableDef {
            name: $t,
            surface: $s,
            unique_k1: $u1,
            unique_k2: $u2,
            indexes: &[$($i),*],
        };
    };
}

table!(
    /// Runtimes: scope org id (`""` = public), id ULID, `k1` `<name>@<version>` (unique), `n1` created.
    RUNTIMES, "forge_agent_runtimes", SURFACE, true, false, [Index::ScopeK1]
);
table!(
    /// Agent definitions: scope org id, id agent principal id, `k1` lower-cased name (unique),
    /// `k2` agent id (for lookups across orgs), `n1` created.
    DEFS, "forge_agent_defs", SURFACE, true, false, [Index::ScopeK1, Index::K2]
);
table!(
    /// Runs: scope repo id, id ULID, `k1` agent id, `k2` runtime id, `n1` created (ms).
    RUNS, "forge_agent_runs", SURFACE, false, false,
    [Index::ScopeK1, Index::K1, Index::K2, Index::ScopeN1]
);
table!(
    /// Run session events: scope run id, id zero-padded seq, `n1` seq.
    RUN_EVENTS, "forge_agent_run_events", SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Delegated run tokens while a run is live: scope run id, id `token`.
    RUN_TOKENS, "forge_agent_run_tokens", SURFACE, false, false, []
);
table!(
    /// The dispatch queue: scope `""`, id run id, `k1` repo id, `n1` queued (ms).
    QUEUE, "forge_agent_queue", SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Provenance sessions: scope repo id, id session id, `k1` agent id, `n1` created.
    SESSIONS, "forge_agent_sessions", SURFACE, false, false, [Index::ScopeK1, Index::K1]
);
table!(
    /// Session links: scope repo id, id `cs:<hex>` or `cr:<id>`, `k1` session id.
    SESSION_LINKS, "forge_agent_session_links", SURFACE, false, false, [Index::ScopeK1]
);
table!(
    /// Change requests authored by agents: scope agent id, id CR id, `k1` repo id, `n1` created.
    AGENT_CRS, "forge_agent_crs", SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Reviews submitted by agents: scope agent id, id review id, `k1` repo id, `n1` created.
    AGENT_REVIEWS, "forge_agent_reviews", SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Counters (`n1`): anomaly sequences per repo.
    COUNTERS, "forge_agent_counters", SURFACE, false, false, []
);
table!(
    /// Owner spending per month: scope org id, id `<owner>/<YYYY-MM>`.
    SPEND, "forge_agent_spend", SURFACE, false, false, []
);
table!(
    /// Owner budgets: scope org id, id owner principal id.
    OWNER_BUDGETS, "forge_agent_owner_budgets", SURFACE, false, false, []
);
table!(
    /// Memories: scope `repo:<id>`, `org:<id>` or `agent:<id>`, id the key, `n1` expiry (ms).
    MEMORIES, "forge_agent_memories", SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Context bundles: scope `repo:<id>` or `org:<id>`, id ULID, `k1` owner id, `n1` created.
    BUNDLES, "forge_agent_bundles", SURFACE, false, false, [Index::ScopeK1]
);
table!(
    /// Watchers: scope repo id, id ULID, `k1` lower-cased name (unique), `n1` created.
    WATCHERS, "forge_agent_watchers", SURFACE, true, false, [Index::ScopeK1]
);
table!(
    /// Debounced watcher triggers: scope `""`, id watcher id, `k1` repo id, `n1` due (ms).
    TRIGGERS, "forge_agent_triggers", SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Anomalies: scope repo id, id ULID, `k1` watcher id, `k2` fingerprint, `n1` seq.
    ANOMALIES, "forge_agent_anomalies", SURFACE, false, false,
    [Index::ScopeK1, Index::ScopeK2, Index::ScopeN1]
);
table!(
    /// Automatic review configuration: scope repo id, id `config`.
    REVIEW_CONFIG, "forge_agent_review_config", SURFACE, false, false, []
);
table!(
    /// Last seen ref tips (for watcher diffs): scope repo id, id ref name.
    REF_TIPS, "forge_agent_ref_tips", SURFACE, false, false, []
);
table!(
    /// Locator: scope kind (`runtime`, `run`, `bundle`, `idem:<principal>`), id object id or
    /// key, `k1` the owning scope.
    LOCATOR, "forge_agent_locator", SURFACE, false, false, []
);

table!(
    /// Feed triggers: scope repo id, id agent id, `k1` agent id (an agent's triggers across
    /// repos), `n1` created (ms).
    FEED_TRIGGERS, "forge_agent_feed_triggers", SURFACE, false, false, [Index::K1]
);
table!(
    /// Debounced feed triggers: scope `""`, id `<repo id>/<agent id>`, `n1` due (ms).
    FEED_PENDING, "forge_agent_feed_pending", SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Posts a feed trigger has taken (one run per post): scope `<repo id>/<agent id>`, id post
    /// id, `n1` when (ms).
    FEED_SEEN, "forge_agent_feed_seen", SURFACE, false, false, []
);

table!(
    /// Workspaces: scope org id, id ULID, `k1` `<owner>/<name>` (unique), `k2` repo id, `n1` created.
    WORKSPACES, "forge_workspaces", WS_SURFACE, true, false, [Index::ScopeK2, Index::ScopeN1]
);
table!(
    /// Locator: scope `ws`, `owner:<id>` or `idem:<principal>`, id workspace id or key, `k1` org id or workspace id.
    WS_LOCATOR, "forge_workspace_locator", WS_SURFACE, false, false, []
);
table!(
    /// Prebuild configurations: scope repo id, id ULID, `n1` created.
    PREBUILD_CONFIGS, "forge_workspace_prebuild_configs", WS_SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Prebuilds: scope config id, id ULID, `k1` repo id, `k2` state name, `n1` created.
    PREBUILDS, "forge_workspace_prebuilds", WS_SURFACE, false, false,
    [Index::ScopeN1, Index::K2]
);
table!(
    /// Dotfiles settings: scope `""`, id principal id.
    DOTFILES, "forge_workspace_dotfiles", WS_SURFACE, false, false, []
);
table!(
    /// Egress policies: scope `org:<id>`, `repo:<id>` or `ws:<id>`, id `policy`.
    EGRESS, "forge_workspace_egress", WS_SURFACE, false, false, []
);
table!(
    /// Usage lines: scope org id, id ULID, `k1` principal id, `n1` time (ms).
    WS_USAGE, "forge_workspace_usage", WS_SURFACE, false, false, [Index::ScopeN1]
);
table!(
    /// Org workspace policy: scope org id, id `policy`.
    WS_POLICY, "forge_workspace_org_policy", WS_SURFACE, false, false, []
);

static M1: Migration = Migration {
    module: "agents",
    version: 1,
    name: "create_agents",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&RUNTIMES),
        MigrationOp::CreateTable(&DEFS),
        MigrationOp::CreateTable(&RUNS),
        MigrationOp::CreateTable(&RUN_EVENTS),
        MigrationOp::CreateTable(&RUN_TOKENS),
        MigrationOp::CreateTable(&QUEUE),
        MigrationOp::CreateTable(&SESSIONS),
        MigrationOp::CreateTable(&SESSION_LINKS),
        MigrationOp::CreateTable(&AGENT_CRS),
        MigrationOp::CreateTable(&AGENT_REVIEWS),
        MigrationOp::CreateTable(&COUNTERS),
        MigrationOp::CreateTable(&SPEND),
        MigrationOp::CreateTable(&OWNER_BUDGETS),
        MigrationOp::CreateTable(&MEMORIES),
        MigrationOp::CreateTable(&BUNDLES),
        MigrationOp::CreateTable(&WATCHERS),
        MigrationOp::CreateTable(&TRIGGERS),
        MigrationOp::CreateTable(&ANOMALIES),
        MigrationOp::CreateTable(&REVIEW_CONFIG),
        MigrationOp::CreateTable(&REF_TIPS),
        MigrationOp::CreateTable(&LOCATOR),
    ],
};

static M2: Migration = Migration {
    module: "agents",
    version: 2,
    name: "create_workspaces",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&WORKSPACES),
        MigrationOp::CreateTable(&WS_LOCATOR),
        MigrationOp::CreateTable(&PREBUILD_CONFIGS),
        MigrationOp::CreateTable(&PREBUILDS),
        MigrationOp::CreateTable(&DOTFILES),
        MigrationOp::CreateTable(&EGRESS),
        MigrationOp::CreateTable(&WS_USAGE),
        MigrationOp::CreateTable(&WS_POLICY),
    ],
};

static M3: Migration = Migration {
    module: "agents",
    version: 3,
    name: "create_feed_triggers",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&FEED_TRIGGERS),
        MigrationOp::CreateTable(&FEED_PENDING),
        MigrationOp::CreateTable(&FEED_SEEN),
    ],
};

/// What the agents module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "agents",
    surfaces: &[SURFACE, WS_SURFACE],
    // Safety interventions share the agents surface so they can interrupt work atomically.
    migrations: &[
        &M1,
        &M2,
        &M3,
        &crate::swarms::MIGRATION,
        &crate::safety::MIGRATION,
    ],
    event_kinds: &[
        EventKind::new(
            "agent_runtime.registered",
            "An agent runtime was registered",
        ),
        EventKind::new("agent_runtime.updated", "An agent runtime was changed"),
        EventKind::new("agent_runtime.deleted", "An agent runtime was deleted"),
        EventKind::new("agent.defined", "An agent definition was created"),
        EventKind::new("agent.updated", "An agent definition was changed"),
        EventKind::new("agent.deleted", "An agent definition was deleted"),
        EventKind::new("agent.budget_updated", "An owner's agent budget changed"),
        EventKind::new("agent_run.queued", "An agent run was queued"),
        EventKind::new("agent_run.started", "An agent run started"),
        EventKind::new(
            "agent_run.state_changed",
            "An agent run moved to another state",
        ),
        EventKind::new("agent_run.completed", "An agent run finished"),
        EventKind::new("agent_run.refused", "An agent run was refused (budget)"),
        EventKind::new("agent_run.message", "A steering message was sent to a run"),
        EventKind::new(
            "agent_memory.updated",
            "An agent memory was written or deleted",
        ),
        EventKind::new("context_bundle.updated", "A context bundle changed"),
        EventKind::new("watcher.updated", "A component watcher changed"),
        EventKind::new(
            "watcher.triggered",
            "Changes in a watched component triggered a run",
        ),
        EventKind::new(
            "agent_feed_trigger.updated",
            "An agent's feed trigger was configured",
        ),
        EventKind::new(
            "agent_feed_trigger.fired",
            "Posts from followed repos started (or were skipped for) an adopt run",
        ),
        EventKind::new("anomaly.reported", "A watcher reported an anomaly"),
        EventKind::new(
            "anomaly.updated",
            "An anomaly was acknowledged, resolved or linked",
        ),
        EventKind::new(
            "agent_review_config.updated",
            "Automatic agent review changed",
        ),
        EventKind::new("workspace.created", "A cloud workspace was created"),
        EventKind::new("workspace.state_changed", "A workspace changed state"),
        EventKind::new("workspace.updated", "A workspace's settings changed"),
        EventKind::new("workspace.deleted", "A workspace was deleted"),
        EventKind::new("workspace.exported", "A workspace's changes were exported"),
        EventKind::new(
            "workspace_port.updated",
            "A forwarded port's visibility changed",
        ),
        EventKind::new(
            "workspace_prebuild.updated",
            "A prebuild configuration or prebuild changed",
        ),
        EventKind::new(
            "workspace_policy.updated",
            "A workspace egress or org policy changed",
        ),
        EventKind::new(
            "workspace_dotfiles.updated",
            "A principal's dotfiles setting changed",
        ),
    ],
    audit_prefixes: &["agent.", "workspace."],
};

/// Settings of the agents module.
#[derive(Clone, Debug)]
pub struct AgentsConfig {
    /// Runs executing at once (the dispatcher's bound).
    pub max_concurrent_runs: usize,
    /// Debounce window of watcher triggers when a watcher sets no `check_interval_seconds`, and
    /// of feed triggers (their `check_interval_seconds`).
    pub watch_debounce: Duration,
    /// Lifetime of delegated run tokens.
    pub run_token_ttl: Duration,
    /// Dispatcher poll interval (it also wakes when a run is queued).
    pub poll: Duration,
    /// Public URL of this deployment (run and workspace links).
    pub base_url: String,
    /// Workspaces.
    pub workspaces: WorkspaceConfig,
}

impl Default for AgentsConfig {
    fn default() -> AgentsConfig {
        AgentsConfig {
            max_concurrent_runs: 8,
            watch_debounce: Duration::from_secs(60),
            run_token_ttl: Duration::from_secs(4 * 3600),
            poll: Duration::from_secs(1),
            base_url: "http://localhost:8080".into(),
            workspaces: WorkspaceConfig::default(),
        }
    }
}

/// What the server backs (see [`ports`]).
#[derive(Clone)]
pub struct AgentPorts {
    /// Executes runs.
    pub runner: Arc<dyn AgentRunner>,
    /// Issues delegated run tokens.
    pub issuer: Arc<dyn RunTokenIssuer>,
    /// Stores logs and transcripts.
    pub blobs: Arc<dyn AgentBlobs>,
    /// Reads repo content and writes workspace changesets.
    pub repo: Arc<dyn AgentRepo>,
    /// Provisions workspaces.
    pub workspaces: Arc<dyn WorkspaceProvider>,
}

/// The agents module over one forge.
pub struct Agents {
    /// The forge.
    pub forge: Arc<Forge>,
    /// Change requests (runs open them; CR outcomes finish runs).
    pub changes: Arc<Changes>,
    /// Cloud workspaces.
    pub workspaces: Arc<Workspaces>,
    pub(crate) ports: AgentPorts,
    pub(crate) config: AgentsConfig,
    pub(crate) db: Db,
    pub(crate) wake: Arc<Notify>,
    pub(crate) run_done: Arc<Notify>,
}

impl Agents {
    /// The agents module over `forge` and `changes`.
    pub fn new(
        forge: Arc<Forge>,
        changes: Arc<Changes>,
        ports: AgentPorts,
        config: AgentsConfig,
    ) -> Arc<Agents> {
        let workspaces = Arc::new(Workspaces::new(
            forge.clone(),
            changes.clone(),
            ports.repo.clone(),
            ports.workspaces.clone(),
            config.workspaces.clone(),
            config.base_url.clone(),
        ));
        Arc::new(Agents {
            db: forge.db.clone(),
            forge,
            changes,
            workspaces,
            ports,
            config,
            wake: Arc::new(Notify::new()),
            run_done: Arc::new(Notify::new()),
        })
    }

    /// Registers the event listener (issue assignments, mentions, CR outcomes, reviews,
    /// watchers, prebuild triggers) and installs the agent gate the issues module consults
    /// when an agent is assigned.
    pub fn attach(self: &Arc<Self>) {
        self.forge
            .events
            .add_listener(Arc::new(triggers::AgentListener::new(self.clone())));
        // Refresh the safety record after attributable work changes.
        self.forge
            .events
            .add_listener(Arc::new(crate::safety::SafetyListener::new(self.clone())));
        let mut hooks = self.forge.work.hooks();
        hooks.agents = Arc::new(triggers::RegistryGate::new(self.clone()));
        self.forge.work.set_hooks(hooks);
    }

    /// Adds `AgentService` and `WorkspaceService` to a tonic router builder.
    pub fn register(self: &Arc<Self>, routes: &mut tonic::service::RoutesBuilder) {
        let api = service::AgentApi::new(self.clone());
        routes.add_service(api.agent_server());
        routes.add_service(api.workspace_server());
    }

    /// The store handle.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// One dispatcher pass: fires due schedules, debounced watcher and feed triggers, then runs
    /// up to [`AgentsConfig::max_concurrent_runs`] queued runs to their next resting state.
    /// Returns how many runs it executed.
    pub async fn dispatch_once(self: &Arc<Self>) -> Result<usize> {
        let cx = Cx::system();
        self.fire_schedules(&cx).await?;
        self.fire_due_triggers(&cx).await?;
        self.fire_due_feed_triggers(&cx).await?;
        self.run_queued(&cx).await
    }

    /// Housekeeping: idle workspaces stop, expired ones are deleted, queued and scheduled
    /// prebuilds run, expired memories are removed.
    pub async fn maintain(self: &Arc<Self>) -> Result<()> {
        let cx = Cx::system();
        self.workspaces.maintain(&cx).await?;
        self.sweep_memories(&cx).await?;
        Ok(())
    }

    /// Starts the dispatcher and maintenance workers; [`AgentWorkers::stop`] ends them.
    pub fn spawn_workers(self: &Arc<Self>) -> AgentWorkers {
        let (stop_tx, stop) = watch::channel(false);
        let mut tasks = Vec::new();
        let me = self.clone();
        let mut s = stop.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if *s.borrow() {
                    return;
                }
                let _ = me.dispatch_once().await;
                tokio::select! {
                    _ = tokio::time::sleep(me.config.poll) => {}
                    _ = me.wake.notified() => {}
                    _ = s.changed() => {}
                }
            }
        }));
        let me = self.clone();
        let mut s = stop;
        tasks.push(tokio::spawn(async move {
            loop {
                if *s.borrow() {
                    return;
                }
                let _ = me.maintain().await;
                tokio::select! {
                    _ = tokio::time::sleep(me.forge.config.maintenance_interval) => {}
                    _ = s.changed() => {}
                }
            }
        }));
        AgentWorkers {
            stop: stop_tx,
            tasks,
        }
    }
}

/// The running agent workers.
pub struct AgentWorkers {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl AgentWorkers {
    /// Signals the workers to stop and waits for them.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

// ---- shared helpers ----

/// Zero-padded decimal, so text keys sort numerically.
pub(crate) fn pad(n: u64) -> String {
    format!("{n:020}")
}

/// Lower-cased trimmed name used as a unique key.
pub(crate) fn lc(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Validates a display name: 1..=100 characters, no control characters.
pub(crate) fn check_name(field: &str, s: &str) -> Result<String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(crate::error::ForgeError::missing(field));
    }
    if t.chars().count() > 100 || t.chars().any(char::is_control) {
        return Err(crate::error::ForgeError::invalid(
            "invalid_name",
            format!("{field} must be 1 to 100 printable characters"),
        ));
    }
    Ok(t.to_string())
}

/// Fails with `TOO_LARGE` when `s` is longer than `max` bytes.
pub(crate) fn check_len(field: &str, s: &str, max: usize) -> Result<()> {
    if s.len() > max {
        return Err(crate::error::ForgeError::too_large(
            field,
            max as u64,
            s.len() as u64,
        ));
    }
    Ok(())
}

/// Proto3-JSON of a message, empty on failure.
pub(crate) fn pjson<T: serde::Serialize>(m: &T) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

/// The `YYYY-MM-DD` UTC date of a millisecond timestamp.
pub(crate) fn utc_date(ms: u64) -> String {
    let (y, m, d) = civil(ms / 86_400_000);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The `YYYY-MM` UTC month of a millisecond timestamp.
pub(crate) fn utc_month(ms: u64) -> String {
    let (y, m, _) = civil(ms / 86_400_000);
    format!("{y:04}-{m:02}")
}

/// Year, month and day of a day count since 1970-01-01 (proleptic Gregorian).
pub(crate) fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_month(1_700_000_000_000), "2023-11");
        assert_eq!(utc_date(1_709_164_800_000), "2024-02-29");
    }
}
