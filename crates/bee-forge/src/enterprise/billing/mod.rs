//! Billing: plans, seats, usage metering, budgets and spending limits, invoices and cost
//! centers (PRODUCT §4.20). Serves `BillingService`. Optional for self-hosters.
//!
//! Owns the `billing` surface. The design is in `docs/spec/orgs-apps.md`; in short:
//!
//! - **Accounts.** An account is an org or an enterprise ([`ResolvedScope`]); its plan,
//!   payment method summary and email settings are rows keyed by the scope key.
//! - **Metering.** [`Billing::record_usage`] stores one row per metered event in the usage
//!   table of the org that incurred it, idempotent by key. [`UsageIngestor`] feeds the same
//!   call from `usage.recorded` events.
//! - **Pricing.** Queries, budgets, enforcement and invoices all read one deterministic
//!   ledger: the month's rows in `(time, id)` order, priced per SKU from the price table in
//!   force, with the plan's included usage consumed first-come per product per month.
//! - **Enforcement.** [`Billing`] implements [`SpendGate`]; [`Billing::enforce`] is the form
//!   the server calls before metered work.
//! - **Self-hosted.** With [`BillingConfig::enabled`] false nothing is billed: usage is
//!   still metered and budgets and cost centers still work, but plan changes, invoices, the
//!   payment method and email settings answer `UNIMPLEMENTED` / `billing_disabled`.

mod budgets;
mod costcenters;
mod invoices;
mod plans;
mod records;
mod service;
mod settings;
pub mod time;
mod usage;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use bee_proto::v1::{EnterpriseScope, RepoRef, SubjectKind, enterprise_scope, repo_ref};
use serde_json::json;

use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::enterprise::common::{
    OrgScopeResolver, ResolvedScope, ScopeKind, ScopeResolver, audit_enterprise, emit,
};
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Db, Index, Migration, MigrationOp, Phase, Surface, TableDef, Tx};

pub use budgets::{BudgetState, LimitState};
pub use invoices::InvoiceUrl;
pub use plans::{PlanChange, SeatInfo, builtin_plans, default_prices};
pub use service::BillingService;
pub use settings::PaymentMethodInput;
pub use usage::{
    UsageIngestor, UsageInput, UsageOutcome, UsageQuery, UsageRecord, UsageReport,
    parse_product_name,
};

/// The billing surface.
pub const SURFACE: Surface = Surface("billing");

/// Account rows by scope key: `plan` (account plan), `payment`, `email`.
pub static ACCOUNTS: TableDef = TableDef {
    name: "forge_billing_accounts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Metered usage, scoped by org id; `k1` idempotency key (unique per org), `n1` time (ms).
pub static USAGE: TableDef = TableDef {
    name: "forge_billing_usage",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::ScopeK1],
};

/// Budgets, scoped by account scope key.
pub static BUDGETS: TableDef = TableDef {
    name: "forge_billing_budgets",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Fired budget alerts, scoped by budget id; id `<yyyymm>:<percent>`.
pub static ALERTS: TableDef = TableDef {
    name: "forge_billing_alerts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Invoices, scoped by account scope key; `k1` status, `k2` period start (unique), `n1`
/// period start (ms).
pub static INVOICES: TableDef = TableDef {
    name: "forge_billing_invoices",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: true,
    indexes: &[Index::ScopeN1, Index::ScopeK1, Index::ScopeK2],
};

/// Cost centers, scoped by account scope key.
pub static COST_CENTERS: TableDef = TableDef {
    name: "forge_billing_cost_centers",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Which cost center owns a repo, org or principal; scoped by account scope key, id
/// `repo:<id>` / `org:<id>` / `principal:<id>`, `k1` cost center id.
pub static CC_ASSIGN: TableDef = TableDef {
    name: "forge_billing_cc_assignments",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Counters (invoice numbers), scope `""`.
pub static COUNTERS: TableDef = TableDef {
    name: "forge_billing_counters",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

static M1: Migration = Migration {
    module: "billing",
    version: 1,
    name: "create_billing_tables",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&ACCOUNTS),
        MigrationOp::CreateTable(&USAGE),
        MigrationOp::CreateTable(&BUDGETS),
        MigrationOp::CreateTable(&ALERTS),
        MigrationOp::CreateTable(&INVOICES),
        MigrationOp::CreateTable(&COST_CENTERS),
        MigrationOp::CreateTable(&CC_ASSIGN),
        MigrationOp::CreateTable(&COUNTERS),
    ],
};

/// What the billing module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "billing",
    surfaces: &[SURFACE],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::new(
            "usage.recorded",
            "Metered usage to ingest (payload: product, sku, quantity, unit, principal_id, repo_id, key)",
        ),
        EventKind::new("billing.plan_changed", "An account's plan or seats changed"),
        EventKind::new("billing.budget_created", "A budget was created"),
        EventKind::new("billing.budget_updated", "A budget was changed"),
        EventKind::new("billing.budget_deleted", "A budget was deleted"),
        EventKind::new(
            "billing.budget_threshold_reached",
            "Spend crossed an alert threshold of a budget (once per threshold per month)",
        ),
        EventKind::new("billing.invoice_created", "An invoice was generated"),
        EventKind::new("billing.invoice_updated", "An invoice changed status"),
        EventKind::new(
            "billing.cost_center_changed",
            "A cost center or its resources changed",
        ),
        EventKind::new(
            "billing.payment_method_updated",
            "The payment method summary changed",
        ),
        EventKind::new(
            "billing.email_settings_updated",
            "Billing email settings changed",
        ),
    ],
    audit_prefixes: &["billing."],
};

/// Billing settings.
#[derive(Clone, Debug)]
pub struct BillingConfig {
    /// Whether anything is billed. When false (self-hosted) usage is still metered, budgets
    /// and cost centers still work, and `GetAccountBillingPlan` reports `billing_disabled`;
    /// `ChangeBillingPlan`, invoices, the payment method and email settings answer
    /// `UNIMPLEMENTED` with subcode `billing_disabled`.
    pub enabled: bool,
    /// The plan catalog; empty means [`builtin_plans`].
    pub plans: Vec<bee_proto::v1::BillingPlan>,
    /// Price table, SKU to micro-USD per unit. Entries override [`default_prices`]; a SKU in
    /// neither table is metered at price 0.
    pub prices: BTreeMap<String, u64>,
    /// Monthly spending limit (micro-USD) applied to an account when no budget covers a
    /// charge; 0 means none.
    pub default_spending_limit_micro_usd: u64,
    /// HMAC key signing invoice PDF URLs. When empty, [`Billing::new`] generates a random one
    /// (URLs then do not survive a restart).
    pub invoice_url_key: Vec<u8>,
    /// Base URL of invoice PDF links, without a trailing slash.
    pub invoice_base_url: String,
    /// Lifetime of a signed invoice URL in seconds.
    pub invoice_url_ttl_secs: u64,
    /// Days from issue to due date.
    pub invoice_due_days: u32,
    /// Tax rate in basis points applied to the invoice subtotal (0 = none).
    pub tax_rate_bps: u32,
    /// Seconds behind the clock before which usage is considered complete
    /// (`data_complete_until`).
    pub usage_lag_secs: u64,
    /// Where the payment method is changed.
    pub portal_url: String,
    /// Principals who may put an account on a plan that is not self-serve: programs such as
    /// `education` and `nonprofit` (a discount on Team, granted after eligibility is checked
    /// outside the product) and contracted plans. The server's site administrators.
    pub program_admins: BTreeSet<String>,
}

impl Default for BillingConfig {
    fn default() -> BillingConfig {
        BillingConfig {
            enabled: true,
            plans: Vec::new(),
            prices: BTreeMap::new(),
            default_spending_limit_micro_usd: 0,
            invoice_url_key: Vec::new(),
            invoice_base_url: "https://beehive.example".into(),
            invoice_url_ttl_secs: 300,
            invoice_due_days: 30,
            tax_rate_bps: 0,
            usage_lag_secs: 300,
            portal_url: String::new(),
            program_admins: BTreeSet::new(),
        }
    }
}

/// Billing over the store: plans and seats, metering, budgets, invoices, cost centers.
pub struct Billing {
    pub(crate) forge: Arc<Forge>,
    pub(crate) db: Db,
    pub(crate) cfg: BillingConfig,
    resolver: RwLock<Arc<dyn ScopeResolver>>,
}

impl Billing {
    /// Billing over `forge`. Scopes resolve to orgs only until
    /// [`Billing::set_scope_resolver`] installs the enterprise-aware resolver.
    pub fn new(forge: Arc<Forge>, mut cfg: BillingConfig) -> Arc<Billing> {
        if cfg.invoice_url_key.is_empty() {
            cfg.invoice_url_key = forge
                .db
                .ids()
                .random_hex(32)
                .unwrap_or_else(|_| "unkeyed".into())
                .into_bytes();
        }
        let resolver: Arc<dyn ScopeResolver> = Arc::new(OrgScopeResolver::new(forge.clone()));
        Arc::new(Billing {
            db: forge.db.clone(),
            forge,
            cfg,
            resolver: RwLock::new(resolver),
        })
    }

    /// Installs the resolver used to find the enterprise an org belongs to and the orgs of an
    /// enterprise.
    pub fn set_scope_resolver(&self, r: Arc<dyn ScopeResolver>) {
        if let Ok(mut g) = self.resolver.write() {
            *g = r;
        }
    }

    /// The settings in force.
    pub fn config(&self) -> &BillingConfig {
        &self.cfg
    }

    pub(crate) fn resolver(&self) -> Arc<dyn ScopeResolver> {
        match self.resolver.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// `UNIMPLEMENTED` / `billing_disabled` unless billing is enabled.
    pub(crate) fn require_enabled(&self) -> Result<()> {
        if self.cfg.enabled {
            Ok(())
        } else {
            Err(
                ForgeError::unimplemented("billing is disabled on this deployment")
                    .with_subcode("billing_disabled"),
            )
        }
    }

    /// The price of one unit of `sku` in micro-USD.
    pub fn unit_price(&self, sku: &str) -> u64 {
        self.cfg
            .prices
            .get(sku)
            .copied()
            .or_else(|| default_prices().get(sku).copied())
            .unwrap_or(0)
    }

    /// The scopes whose budgets and limits cover usage of `org_id`: the org itself, then its
    /// enterprise account if it has one.
    pub(crate) async fn scopes_of_org(&self, cx: &Cx, org_id: &str) -> Result<Vec<ResolvedScope>> {
        let resolver = self.resolver();
        let org = resolver.resolve_org(cx, org_id).await?;
        let mut out = vec![org.clone()];
        if let Some(e) = &org.enterprise_id {
            let scope = Some(EnterpriseScope {
                scope: Some(enterprise_scope::Scope::Enterprise(e.clone())),
            });
            out.push(resolver.resolve(cx, &scope).await?);
        }
        Ok(out)
    }

    /// An org of `scope` by name or id (`NOT_FOUND` when it is not part of the scope).
    pub(crate) async fn org_in_scope(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        name_or_id: &str,
    ) -> Result<String> {
        let org = self.forge.orgs.get_org(cx, name_or_id).await?;
        if scope.org_ids.contains(&org.id) {
            Ok(org.id)
        } else {
            Err(ForgeError::not_found(format!(
                "org {name_or_id} in this account"
            )))
        }
    }

    /// The id of a repo of `scope` (`NOT_FOUND` when it is not part of the scope).
    pub(crate) async fn repo_in_scope(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        r: &RepoRef,
    ) -> Result<String> {
        let repo = self.forge.orgs.resolve_repo(cx, r).await?;
        if scope.org_ids.contains(&repo.org_id) {
            Ok(repo.id)
        } else {
            let what = match &r.repo {
                Some(repo_ref::Repo::Id(s)) | Some(repo_ref::Repo::FullName(s)) => s.clone(),
                None => String::new(),
            };
            Err(ForgeError::not_found(format!(
                "repo {what} in this account"
            )))
        }
    }
}

/// One change to an account: the event it emits and the audit entry it writes.
pub(crate) struct Change<'a> {
    /// Event kind.
    pub kind: &'a str,
    /// Audit action.
    pub action: &'a str,
    /// Id of the changed object.
    pub target: &'a str,
    /// Proto3-JSON of the object before, if it existed.
    pub before: Option<&'a str>,
    /// Proto3-JSON of the object after.
    pub resource: &'a str,
    /// Org whose event log receives the event (empty for the system log).
    pub event_org: &'a str,
}

/// Emits the event and audits the change to an account, in the caller's transaction.
pub(crate) async fn record_change(tx: &mut Tx, scope: &ResolvedScope, c: Change<'_>) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(c.resource).unwrap_or(serde_json::Value::Null);
    let payload = json!({ "scope": scope.key(), "id": c.target, "resource": value }).to_string();
    emit(
        tx,
        c.kind,
        c.event_org,
        Some((SubjectKind::Unspecified, c.target)),
        payload,
    )
    .await?;
    audit_change(tx, scope, c.action, c.target, c.before, c.resource).await
}

/// Audits a change to an account in the org's chain, or the enterprise's.
pub(crate) async fn audit_change(
    tx: &mut Tx,
    scope: &ResolvedScope,
    action: &str,
    target: &str,
    before: Option<&str>,
    after: &str,
) -> Result<()> {
    let mut draft = AuditDraft::new(action, &scope.audit_org())
        .target_id(target)
        .after(after);
    if let Some(b) = before {
        draft = draft.before(b);
    }
    match scope.kind {
        ScopeKind::Org => tx.audit(draft).await,
        ScopeKind::Enterprise => audit_enterprise(tx, &scope.id, draft).await,
    }
}

/// Event log org for a change to `scope`: the org itself, empty for an enterprise.
pub(crate) fn event_org(scope: &ResolvedScope) -> String {
    scope.audit_org()
}
