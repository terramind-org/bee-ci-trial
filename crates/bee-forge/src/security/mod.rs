//! Security: dependency graph, advisories and alerts, update change requests, code and secret
//! scanning, push protection and its bypasses, repo security advisories and the overview
//! (PRODUCT §4.18). Serves `SecurityService`. The contract is `docs/spec/security.md`.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | the module declaration (surface, tables, migration, event kinds, audit prefixes), [`Security`] and shared helpers |
//! | [`model`] | crate-private stored records |
//! | [`ports`] | what the module needs from the rest of the system, with defaults and in-memory implementations |
//! | [`vcs`] | [`vcs::HistorySecurityVcs`], the [`ports::SecurityVcs`] over `bee-history` |
//! | [`depgraph`] | graphs at a revision, manifests, snapshots, dependents, SBOMs, dependency review |
//! | [`advisories`] | the advisory index (OSV feed + published advisories) and incremental re-evaluation |
//! | [`alerts`] | vulnerability alerts: evaluation, dismissal, the ref and advisory triggers |
//! | [`updates`] | the update configuration, update runs and update change requests |
//! | [`lockfile`] | the built-in lockfile refresher |
//! | [`secrets`] | secret scanning: patterns, alerts, validity, push protection, bypasses |
//! | [`codescan`] | SARIF ingest, alert dedupe by fingerprint, analyses, check runs, code quality |
//! | [`repo_advisories`] | repository security advisories, private reports, CVE requests |
//! | [`overview`] | configuration, overview and campaigns |
//! | [`service`] | the tonic `SecurityService` ([`service::SecurityServices`]) |
//!
//! # Storage
//!
//! Everything lives on the `security` surface; every state change is one transaction that also
//! records its events and audit entry (`docs/spec/forge-core.md` §2.3).

pub mod advisories;
pub mod alerts;
pub mod codescan;
pub mod depgraph;
pub mod lockfile;
pub mod model;
pub mod overview;
pub mod ports;
pub mod repo_advisories;
pub mod secrets;
pub mod service;
pub mod updates;
pub mod vcs;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::Notify;

use async_trait::async_trait;
use bee_proto::v1::{
    Event, ObjectId, Repo, RepoRef, SecuritySeverity, SubjectKind, SubjectRef, event, repo_ref,
};

pub use ports::Ports;
pub use service::SecurityServices;

use crate::audit::AuditDraft;
use crate::changes::Changes;
use crate::checks::Checks;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventListener;
use crate::forge::Forge;
use crate::hooks::PrincipalDirectory;
use crate::module::{EventKind, ForgeModule};
use crate::orgs::Orgs;
use crate::store::{Db, Index, Migration, MigrationOp, Phase, Surface, TableDef, Tx};

/// The security surface.
pub const SURFACE: Surface = Surface("security");

macro_rules! table {
    ($(#[$m:meta])* $id:ident, $name:literal, $k1:expr, $k2:expr, [$($idx:expr),*]) => {
        $(#[$m])*
        pub static $id: TableDef = TableDef {
            name: $name,
            surface: SURFACE,
            unique_k1: $k1,
            unique_k2: $k2,
            indexes: &[$($idx),*],
        };
    };
}

table!(
    /// Settings: scope repo or org id, id the setting (`config`, `code_setup`, `validity`,
    /// `bypass_policy`, `update_cfg`, `update_state`).
    CONFIG, "forge_sec_config", false, false, [Index::ScopeK1, Index::K1]);
table!(
    /// Counters (`n1`): per-repo alert numbers by kind.
    COUNTERS, "forge_sec_counters", false, false, []);
table!(
    /// The last evaluated graph per repo: scope `""`, id the repo id.
    GRAPHS, "forge_sec_graphs", false, false, [Index::ScopeN1]);
table!(
    /// Submitted dependency snapshots: scope repo, id `<detector>:<correlator>`.
    SNAPSHOTS, "forge_sec_snapshots", false, false, [Index::ScopeN1]);
table!(
    /// Vulnerability alerts: scope repo, id the zero-padded number, `k1` the dedupe key
    /// (unique), `k2` the state, `n1` the update time.
    DEP_ALERTS, "forge_sec_dep_alerts", true, false, [Index::ScopeK2, Index::ScopeN1]);
table!(
    /// Secret alerts: scope repo, id the zero-padded number, `k1` the dedupe key (unique), `k2`
    /// the state, `n1` the update time.
    SECRET_ALERTS, "forge_sec_secret_alerts", true, false, [Index::ScopeK2, Index::ScopeN1]);
table!(
    /// Custom secret patterns: scope the repo or org id, id a ULID, `k1` the lower-cased name
    /// (unique per scope), `k2` the id (for lookups across scopes).
    PATTERNS, "forge_sec_patterns", true, false, [Index::K2]);
table!(
    /// Push-protection blocks: scope repo, id the block id, `n1` the expiry.
    BLOCKS, "forge_sec_blocks", false, false, [Index::ScopeN1, Index::N1]);
table!(
    /// Bypass requests: scope repo, id a ULID, `k1` the requester, `k2` the state, `n1` the
    /// creation time.
    BYPASSES, "forge_sec_bypasses", false, false, [Index::ScopeK1, Index::ScopeK2, Index::ScopeN1, Index::K2]);
table!(
    /// Code scanning alerts: scope repo, id the zero-padded number, `k1` the dedupe key
    /// (unique), `k2` the state, `n1` the update time.
    CODE_ALERTS, "forge_sec_code_alerts", true, false, [Index::ScopeK2, Index::ScopeN1]);
table!(
    /// Alert instances: scope `<repo id>:<number>`, id `<ref>|<category>`, `k1` the ref.
    CODE_INSTANCES, "forge_sec_code_instances", false, false, [Index::ScopeK1]);
table!(
    /// Analyses: scope repo, id a ULID, `k1` the ref, `k2` the tool, `n1` the creation time.
    ANALYSES, "forge_sec_analyses", false, false, [Index::ScopeK1, Index::ScopeK2, Index::ScopeN1]);
table!(
    /// SARIF uploads: scope repo, id a ULID, `k2` the status, `n1` the creation time.
    SARIF, "forge_sec_sarif", false, false, [Index::K2, Index::ScopeN1]);
table!(
    /// Code quality findings: scope repo, id the zero-padded number, `k1` the dedupe key
    /// (unique), `k2` the state.
    QUALITY, "forge_sec_quality", true, false, [Index::ScopeK2]);
table!(
    /// Repo advisories: scope repo, id the GHSA id, `k1` the state, `n1` the update time.
    REPO_ADVISORIES, "forge_sec_repo_advisories", false, false, [Index::ScopeK1, Index::ScopeN1]);
table!(
    /// Published advisories of the global database that did not come from the feed: scope `""`,
    /// id the GHSA id.
    GLOBAL_ADVISORIES, "forge_sec_global_advisories", false, false, [Index::ScopeN1]);
table!(
    /// CVE requests: scope `""`, id a ULID, `k1` the GHSA id.
    CVE_REQUESTS, "forge_sec_cve_requests", false, false, [Index::ScopeK1]);
table!(
    /// Update runs: scope repo, id a ULID, `k1` the entry key, `k2` the state, `n1` the
    /// creation time. `k1` is held only while a run is live, so it is the durable per-entry
    /// queue lease shared by every forge replica.
    UPDATE_RUNS, "forge_sec_update_runs", false, false, [Index::ScopeK1, Index::ScopeK2, Index::ScopeN1, Index::K2]);
table!(
    /// Change requests the update bot opened: scope repo, id the update key.
    UPDATE_PRS, "forge_sec_update_prs", false, false, [Index::ScopeN1]);
table!(
    /// Campaigns: scope org id, id a ULID, `k2` the state, `n1` the creation time.
    CAMPAIGNS, "forge_sec_campaigns", false, false, [Index::ScopeK2, Index::ScopeN1]);

static M1: Migration = Migration {
    module: "security",
    version: 1,
    name: "create_security",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&CONFIG),
        MigrationOp::CreateTable(&COUNTERS),
        MigrationOp::CreateTable(&GRAPHS),
        MigrationOp::CreateTable(&SNAPSHOTS),
        MigrationOp::CreateTable(&DEP_ALERTS),
        MigrationOp::CreateTable(&SECRET_ALERTS),
        MigrationOp::CreateTable(&PATTERNS),
        MigrationOp::CreateTable(&BLOCKS),
        MigrationOp::CreateTable(&BYPASSES),
        MigrationOp::CreateTable(&CODE_ALERTS),
        MigrationOp::CreateTable(&CODE_INSTANCES),
        MigrationOp::CreateTable(&ANALYSES),
        MigrationOp::CreateTable(&SARIF),
        MigrationOp::CreateTable(&QUALITY),
        MigrationOp::CreateTable(&REPO_ADVISORIES),
        MigrationOp::CreateTable(&GLOBAL_ADVISORIES),
        MigrationOp::CreateTable(&CVE_REQUESTS),
        MigrationOp::CreateTable(&UPDATE_RUNS),
        MigrationOp::CreateTable(&UPDATE_PRS),
        MigrationOp::CreateTable(&CAMPAIGNS),
    ],
};

static M2: Migration = Migration {
    module: "security",
    version: 2,
    name: "unique_live_update_runs",
    phase: Phase::Expand,
    ops: &[MigrationOp::Sql {
        surface: SURFACE,
        sqlite: "CREATE UNIQUE INDEX IF NOT EXISTS forge_sec_update_runs_uk1 ON forge_sec_update_runs (scope, k1) WHERE k2 IN ('queued', 'running');",
        postgres: "CREATE UNIQUE INDEX IF NOT EXISTS forge_sec_update_runs_uk1 ON forge_sec_update_runs (scope, k1) WHERE k2 IN ('queued', 'running');",
    }],
};

/// What the security module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "security",
    surfaces: &[SURFACE],
    migrations: &[&M1, &M2],
    event_kinds: &[
        EventKind::gh(
            "security_alert.created",
            "A dependency alert was opened",
            "dependabot_alert",
            "created",
        ),
        EventKind::gh(
            "security_alert.fixed",
            "A dependency alert was fixed",
            "dependabot_alert",
            "fixed",
        ),
        EventKind::gh(
            "security_alert.dismissed",
            "A dependency alert was dismissed",
            "dependabot_alert",
            "dismissed",
        ),
        EventKind::gh(
            "security_alert.reopened",
            "A dependency alert was reopened",
            "dependabot_alert",
            "reopened",
        ),
        EventKind::gh(
            "security_alert.auto_dismissed",
            "A dependency alert was dismissed because its advisory was withdrawn",
            "dependabot_alert",
            "auto_dismissed",
        ),
        EventKind::new(
            "dependency_snapshot.submitted",
            "A dependency snapshot was submitted",
        ),
        EventKind::new(
            "dependency_graph.updated",
            "The dependency graph of a repo was re-evaluated",
        ),
        EventKind::new("dependency_update.run_started", "An update run started"),
        EventKind::new("dependency_update.run_finished", "An update run finished"),
        EventKind::new(
            "dependency_update.config_updated",
            "Update switches changed",
        ),
        EventKind::new(
            "dependency_update.change_opened",
            "The update bot opened a change request",
        ),
        EventKind::gh(
            "secret_scanning_alert.created",
            "A secret was found",
            "secret_scanning_alert",
            "created",
        ),
        EventKind::gh(
            "secret_scanning_alert.resolved",
            "A secret alert was resolved",
            "secret_scanning_alert",
            "resolved",
        ),
        EventKind::gh(
            "secret_scanning_alert.reopened",
            "A secret alert was reopened",
            "secret_scanning_alert",
            "reopened",
        ),
        EventKind::gh(
            "secret_scanning_alert.validated",
            "A secret alert's validity changed",
            "secret_scanning_alert",
            "validated",
        ),
        EventKind::new(
            "secret_scanning_pattern.created",
            "A custom secret pattern was created",
        ),
        EventKind::new(
            "secret_scanning_pattern.updated",
            "A custom secret pattern was updated",
        ),
        EventKind::new(
            "secret_scanning_pattern.deleted",
            "A custom secret pattern was deleted",
        ),
        EventKind::new(
            "push_protection.blocked",
            "A write was blocked by push protection",
        ),
        EventKind::new(
            "push_protection.bypass_requested",
            "A push-protection bypass was requested",
        ),
        EventKind::new(
            "push_protection.bypass_reviewed",
            "A push-protection bypass request was decided",
        ),
        EventKind::new(
            "push_protection.bypassed",
            "A push-protection block was bypassed",
        ),
        EventKind::gh(
            "code_scanning_alert.created",
            "A code scanning alert was opened",
            "code_scanning_alert",
            "created",
        ),
        EventKind::gh(
            "code_scanning_alert.fixed",
            "A code scanning alert was fixed",
            "code_scanning_alert",
            "fixed",
        ),
        EventKind::gh(
            "code_scanning_alert.reopened",
            "A code scanning alert was reopened",
            "code_scanning_alert",
            "reopened",
        ),
        EventKind::gh(
            "code_scanning_alert.closed_by_user",
            "A code scanning alert was dismissed",
            "code_scanning_alert",
            "closed_by_user",
        ),
        EventKind::new(
            "code_scanning.analysis_processed",
            "A SARIF upload was processed into an analysis",
        ),
        EventKind::new(
            "code_scanning.autofix_requested",
            "An agent run was requested to fix a code scanning alert",
        ),
        EventKind::new(
            "code_scanning.setup_updated",
            "The default setup of code scanning changed",
        ),
        EventKind::new(
            "code_quality_finding.updated",
            "A code quality finding changed state",
        ),
        EventKind::gh(
            "security_advisory.created",
            "A repo security advisory was drafted",
            "repository_advisory",
            "reported",
        ),
        EventKind::new(
            "security_advisory.updated",
            "A repo security advisory changed",
        ),
        EventKind::gh(
            "security_advisory.published",
            "A repo security advisory was published",
            "repository_advisory",
            "published",
        ),
        EventKind::new(
            "security_advisory.cve_requested",
            "A CVE id was requested for an advisory",
        ),
        EventKind::new(
            "security_advisory.collaborator_changed",
            "A collaborator was added to or removed from an advisory",
        ),
        EventKind::new(
            "security_advisory.reported",
            "A private vulnerability report was submitted",
        ),
        EventKind::new(
            "security_config.updated",
            "Security features of a repo changed",
        ),
        EventKind::new(
            "security_campaign.created",
            "A security campaign was created",
        ),
        EventKind::new("security_campaign.updated", "A security campaign changed"),
    ],
    audit_prefixes: &["security.", "push_protection."],
};

/// Zero-padded number used as a row id so ids sort numerically.
pub(crate) fn num_id(n: u64) -> String {
    format!("{n:020}")
}

/// The wire reference of a repo.
pub(crate) fn repo_ref_of(repo: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(repo.id.clone())),
    })
}

/// A wire severity from the dependency library's.
pub(crate) fn sev(s: bee_deps::cvss::Severity) -> SecuritySeverity {
    use bee_deps::cvss::Severity as S;
    match s {
        S::None => SecuritySeverity::Unspecified,
        S::Low => SecuritySeverity::Low,
        S::Medium => SecuritySeverity::Medium,
        S::High => SecuritySeverity::High,
        S::Critical => SecuritySeverity::Critical,
    }
}

/// The wire severity of a raw value.
pub(crate) fn sev_of(v: i32) -> SecuritySeverity {
    SecuritySeverity::try_from(v).unwrap_or(SecuritySeverity::Unspecified)
}

/// A state change to record: its event and its audit entry.
pub(crate) struct Change<'a> {
    pub repo: &'a Repo,
    pub event: &'a str,
    pub action: &'a str,
    pub subject: SubjectKind,
    pub id: &'a str,
    pub payload: serde_json::Value,
    pub before: String,
    pub after: String,
}

/// Emits the event and writes the audit entry of one state change.
pub(crate) async fn record(tx: &mut Tx, c: Change<'_>) -> Result<()> {
    record_id(tx, c).await.map(|_| ())
}

/// [`record`], returning the audit entry's id.
pub(crate) async fn record_id(tx: &mut Tx, c: Change<'_>) -> Result<String> {
    tx.emit_event(Event {
        kind: c.event.to_string(),
        repo_id: c.repo.id.clone(),
        org_id: c.repo.org_id.clone(),
        subject: Some(SubjectRef {
            kind: c.subject as i32,
            repo: repo_ref_of(c.repo),
            id: c.id.to_string(),
        }),
        payload_json: c.payload.to_string(),
        ..Default::default()
    })
    .await?;
    let mut draft = AuditDraft::new(c.action, &c.repo.org_id)
        .repo_id(&c.repo.id)
        .target(c.subject, c.id);
    if !c.before.is_empty() {
        draft = draft.before(&c.before);
    }
    if !c.after.is_empty() {
        draft = draft.after(&c.after);
    }
    tx.audit_id(draft).await
}

/// Emits the event and writes the audit entry of an org-level change (no repo).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_org(
    tx: &mut Tx,
    org_id: &str,
    event: &str,
    action: &str,
    subject: SubjectKind,
    id: &str,
    payload: serde_json::Value,
    after: String,
) -> Result<()> {
    tx.emit_event(Event {
        kind: event.to_string(),
        org_id: org_id.to_string(),
        subject: Some(SubjectRef {
            kind: subject as i32,
            repo: None,
            id: id.to_string(),
        }),
        payload_json: payload.to_string(),
        ..Default::default()
    })
    .await?;
    let mut draft = AuditDraft::new(action, org_id).target(subject, id);
    if !after.is_empty() {
        draft = draft.after(&after);
    }
    tx.audit(draft).await
}

/// Allocates the next number of `kind` in a repo.
pub(crate) async fn next_number(tx: &mut Tx, repo_id: &str, kind: &str) -> Result<u64> {
    Ok(tx.add_n1(&COUNTERS, repo_id, kind, 1).await? as u64)
}

/// Offset page tokens over an ordered list.
pub(crate) fn page_slice<T: Clone>(
    items: &[T],
    token: &str,
    size: usize,
) -> Result<(Vec<T>, String)> {
    let offset: usize = if token.is_empty() {
        0
    } else {
        token
            .parse()
            .map_err(|_| ForgeError::invalid("bad_page_token", "malformed page token"))?
    };
    let end = (offset + size).min(items.len());
    let slice = items.get(offset..end).unwrap_or_default().to_vec();
    let next = if end < items.len() {
        end.to_string()
    } else {
        String::new()
    };
    Ok((slice, next))
}

/// An RFC 3339 UTC time with seconds from milliseconds since the epoch.
pub(crate) fn rfc3339(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The security module: stores, ports and the advisory index.
pub struct Security {
    pub(crate) db: Db,
    pub(crate) orgs: Arc<Orgs>,
    pub(crate) checks: Arc<Checks>,
    pub(crate) directory: Arc<dyn PrincipalDirectory>,
    pub(crate) ports: Ports,
    pub(crate) changes: Option<Arc<Changes>>,
    pub(crate) index: RwLock<advisories::AdvisoryIndex>,
    pub(crate) scanner: bee_secrets::Scanner,
    pub(crate) sbom_namespace_base: RwLock<Option<String>>,
}

impl Security {
    /// The module over `forge`'s store, orgs, checks and directory. `changes` is the changes
    /// module update runs open their change requests through (`None`: update runs answer
    /// `UNIMPLEMENTED`).
    pub fn new(forge: &Forge, changes: Option<Arc<Changes>>, ports: Ports) -> Arc<Security> {
        Arc::new(Security {
            db: forge.db.clone(),
            orgs: forge.orgs.clone(),
            checks: forge.checks.clone(),
            directory: forge.hooks.directory.clone(),
            ports,
            changes,
            index: RwLock::new(advisories::AdvisoryIndex::default()),
            scanner: bee_secrets::Scanner::builtin(),
            sbom_namespace_base: RwLock::new(None),
        })
    }

    /// Sets the server-resolved base used for exported SBOM namespaces.
    pub fn set_sbom_namespace_base(&self, base: String) {
        if let Ok(mut slot) = self.sbom_namespace_base.write() {
            *slot = Some(base);
        }
    }

    /// Registers the listener that re-evaluates the dependency graph when a default branch
    /// moves (`ref.updated`).
    pub fn attach(self: &Arc<Self>, forge: &Forge) {
        forge
            .events
            .add_listener(Arc::new(RefListener::new(self.clone())));
    }

    /// The ports.
    pub fn ports(&self) -> &Ports {
        &self.ports
    }

    /// One maintenance pass for the server's worker: unprocessed SARIF uploads, advisory feed
    /// reload with incremental re-evaluation, due scheduled updates and expiry of blocks.
    pub async fn maintain(&self, cx: &Cx) -> Result<()> {
        self.process_pending_sarif(cx).await?;
        self.refresh_advisories(cx).await?;
        self.run_due_updates(cx).await?;
        self.expire_blocks(cx).await?;
        Ok(())
    }

    pub(crate) fn now(&self) -> u64 {
        self.db.now_ms()
    }

    pub(crate) fn principal_ref(&self, id: &str) -> bee_proto::v1::Principal {
        bee_proto::v1::Principal {
            id: id.to_string(),
            ..Default::default()
        }
    }

    /// A principal with the directory's details, or just the id.
    pub(crate) async fn principal(&self, id: &str) -> bee_proto::v1::Principal {
        self.directory
            .get(id)
            .await
            .unwrap_or_else(|| self.principal_ref(id))
    }
}

/// Re-evaluates a repo's dependency graph when its default branch moves.
///
/// Bounded: `on_event` only records the newest target per (repo, ref) and wakes one worker
/// task, which evaluates the pending moves one at a time. A burst of lands on one branch
/// therefore costs one evaluation per worker pass, not one per land, and the memory held is
/// one entry per moved ref.
struct RefListener {
    security: Arc<Security>,
    pending: Arc<Mutex<BTreeMap<(String, String), ObjectId>>>,
    wake: Arc<Notify>,
    started: AtomicBool,
}

impl RefListener {
    fn new(security: Arc<Security>) -> RefListener {
        RefListener {
            security,
            pending: Arc::default(),
            wake: Arc::new(Notify::new()),
            started: AtomicBool::new(false),
        }
    }

    fn start_worker(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let (security, pending, wake) = (
            self.security.clone(),
            self.pending.clone(),
            self.wake.clone(),
        );
        tokio::spawn(async move {
            loop {
                let next = pending.lock().ok().and_then(|mut p| p.pop_first());
                let Some(((repo_id, ref_name), target)) = next else {
                    wake.notified().await;
                    continue;
                };
                let cx = Cx::system();
                let Ok(repo) = security.orgs.repo_any(&cx, &repo_id).await else {
                    continue;
                };
                if ref_name == repo.default_ref {
                    let _ = security.on_default_branch_moved(&cx, &repo, &target).await;
                }
            }
        });
    }
}

#[async_trait]
impl EventListener for RefListener {
    async fn on_event(&self, e: &Event) {
        if e.kind != "ref.updated" || e.repo_id.is_empty() {
            return;
        }
        let Some(event::Body::RefUpdated(r)) = &e.body else {
            return;
        };
        let Some(target) = r.target.clone() else {
            return;
        };
        if let Ok(mut p) = self.pending.lock() {
            p.insert((e.repo_id.clone(), r.name.clone()), target);
        }
        self.start_worker();
        self.wake.notify_one();
    }
}

/// Hex of an object id.
pub(crate) fn hex_id(id: &ObjectId) -> String {
    crate::ids::hex(&id.raw)
}
