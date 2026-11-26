//! Usage metering, the priced ledger and `QueryBillingUsage`.
//!
//! # The ledger
//!
//! One function prices usage for everything that needs a number: queries, budgets,
//! enforcement and invoices. For an account scope and a UTC month it reads the usage rows of
//! every org in the scope, orders them by `(time, row id)`, prices each at the SKU's unit
//! price (`floor(quantity * price)` in micro-USD per row, so totals do not depend on how
//! rows are grouped) and consumes the plan's included usage of the product first-come: the
//! covered quantity is the row's discount. The same rows and prices always produce the same
//! ledger.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bee_proto::v1::{
    BillingProduct, BillingUsageGrouping, BillingUsageLineItem, Event, PrincipalKind, RepoRef,
    repo_ref,
};

use super::records::UsageRow;
use super::time::{day_label, month_label, month_of, month_start_ms};
use super::{Billing, BudgetState, CC_ASSIGN, COST_CENTERS, LimitState, SURFACE, USAGE};
use crate::cx::Cx;
use crate::enterprise::common::{MAX_LIST, ResolvedScope, SCAN_CAP, get_or_404};
use crate::error::{ForgeError, Result};
use crate::events::{EventListener, logs_of};
use crate::ids::ts_ms;
use crate::store::{Cursor, Row, Scan, Tx};

const PAGE: usize = 5_000;
const MICRO: u128 = 1_000_000;
/// Longest window a usage query spans, in months.
pub const MAX_QUERY_MONTHS: i64 = 24;

/// One usage event to meter.
#[derive(Clone, Debug, Default)]
pub struct UsageInput {
    /// The org that incurred the usage.
    pub org_id: String,
    /// The repo it was incurred in, or empty.
    pub repo_id: String,
    /// Who did the work (an agent is billed through its owner).
    pub principal_id: String,
    /// What was used.
    pub product: BillingProduct,
    /// The SKU, which selects the unit price (for example `actions_linux_4core`).
    pub sku: String,
    /// Quantity in `unit`s; positive and finite.
    pub quantity: f64,
    /// Unit, for example `minute`, `GB-month`, `token`.
    pub unit: String,
    /// Idempotency key: recording the same key in the same org again is a no-op. Empty means
    /// every call records.
    pub idempotency_key: String,
    /// When the usage happened, ms; 0 means now.
    pub at_ms: u64,
}

/// A stored usage record.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRecord {
    /// Row id (ULID).
    pub id: String,
    /// Org id.
    pub org_id: String,
    /// Repo id, or empty.
    pub repo_id: String,
    /// The principal that did the work.
    pub principal_id: String,
    /// The principal billed: the owner of an agent, else the principal itself.
    pub attributed_to: String,
    /// Product.
    pub product: BillingProduct,
    /// SKU.
    pub sku: String,
    /// Quantity.
    pub quantity: f64,
    /// Unit.
    pub unit: String,
    /// Idempotency key.
    pub key: String,
    /// Time of the usage, ms.
    pub at_ms: u64,
}

/// What recording usage did and the state of every limit that covers it afterwards.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageOutcome {
    /// The stored record (the first one when the key was seen before).
    pub record: UsageRecord,
    /// Whether the idempotency key had been recorded already.
    pub duplicate: bool,
    /// Every budget covering the record, with spend after this charge.
    pub budgets: Vec<BudgetState>,
    /// The default spending limit's state, when no budget covers the record and one is set.
    pub spending_limit: Option<LimitState>,
    /// True when a hard-stop budget or the spending limit is now at or over its amount: the
    /// next metered work will be refused by [`Billing::enforce`].
    pub limit_hit: bool,
}

/// A usage query.
#[derive(Clone, Debug, Default)]
pub struct UsageQuery {
    /// Products to include; empty means all.
    pub products: Vec<BillingProduct>,
    /// Window start (inclusive, ms); `None` is the start of the current month.
    pub start_ms: Option<u64>,
    /// Window end (exclusive, ms); `None` is just after now.
    pub end_ms: Option<u64>,
    /// Grouping dimensions; empty means day and product.
    pub group_by: Vec<BillingUsageGrouping>,
    /// Only this repo.
    pub repo: Option<RepoRef>,
    /// Only this cost center.
    pub cost_center_id: String,
    /// Only this org (name or id).
    pub org: String,
    /// Continue after this token.
    pub after: String,
    /// Page size (0 = default).
    pub limit: usize,
}

/// The answer to a [`UsageQuery`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageReport {
    /// One page of priced lines in key order.
    pub line_items: Vec<BillingUsageLineItem>,
    /// Gross total across all pages, micro-USD.
    pub total_gross_micro_usd: u64,
    /// Net total across all pages, micro-USD.
    pub total_net_micro_usd: u64,
    /// Usage after this time may still arrive, ms.
    pub data_complete_until_ms: u64,
    /// Token of the next page, empty at the end.
    pub next_token: String,
}

/// A priced usage row of the ledger.
#[derive(Clone, Debug)]
pub(crate) struct Priced {
    pub id: String,
    pub org_id: String,
    pub repo_id: String,
    pub principal_id: String,
    pub product: i32,
    pub sku: String,
    pub unit: String,
    pub quantity_micro: u64,
    pub at_ms: u64,
    pub unit_price: u64,
    pub gross: u64,
    pub discount: u64,
    pub net: u64,
    /// Quantity covered by included usage, millionths of a unit.
    pub discount_quantity_micro: u64,
    pub cost_center: String,
}

/// What pricing a scope's ledger needs besides the rows.
pub(crate) struct LedgerCtx {
    /// Included usage per product and month, millionths of a unit.
    pub included: BTreeMap<i32, u64>,
    /// `repo:<id>` / `org:<id>` / `principal:<id>` to cost center id.
    pub assign: BTreeMap<String, String>,
}

pub(crate) fn to_micro(q: f64) -> u64 {
    (q * 1_000_000.0).round() as u64
}

fn mul_div(quantity_micro: u64, price: u64) -> u64 {
    (u128::from(quantity_micro) * u128::from(price) / MICRO) as u64
}

/// The cost center a charge belongs to: its repo, then the principal doing the work (or the
/// principal's owner), then its org, else none (the account itself).
pub(crate) fn cost_center_of(
    assign: &BTreeMap<String, String>,
    repo_id: &str,
    principal_id: &str,
    attributed_to: &str,
    org_id: &str,
) -> String {
    let keys = [
        (!repo_id.is_empty()).then(|| format!("repo:{repo_id}")),
        Some(format!("principal:{principal_id}")),
        Some(format!("principal:{attributed_to}")),
        Some(format!("org:{org_id}")),
    ];
    keys.into_iter()
        .flatten()
        .find_map(|k| assign.get(&k).cloned())
        .unwrap_or_default()
}

/// Parses a product name: `actions_minutes`, `ACTIONS_MINUTES` or `BILLING_PRODUCT_ACTIONS_MINUTES`.
pub fn parse_product_name(s: &str) -> Option<BillingProduct> {
    let up = s.trim().to_ascii_uppercase();
    let full = if up.starts_with("BILLING_PRODUCT_") {
        up
    } else {
        format!("BILLING_PRODUCT_{up}")
    };
    BillingProduct::from_str_name(&full).filter(|p| *p != BillingProduct::Unspecified)
}

fn record_of(org_id: &str, id: &str, r: &UsageRow) -> UsageRecord {
    UsageRecord {
        id: id.to_string(),
        org_id: org_id.to_string(),
        repo_id: r.repo_id.clone(),
        principal_id: r.principal_id.clone(),
        attributed_to: r.attributed_to.clone(),
        product: BillingProduct::try_from(r.product).unwrap_or(BillingProduct::Unspecified),
        sku: r.sku.clone(),
        quantity: r.quantity_micro as f64 / 1_000_000.0,
        unit: r.unit.clone(),
        key: r.key.clone(),
        at_ms: r.at_ms,
    }
}

impl Billing {
    /// The principal billed for work done by `principal_id`: the owner of an agent (from the
    /// principal directory), else the principal itself.
    pub(crate) async fn attributed_to(&self, principal_id: &str) -> String {
        if principal_id.is_empty() {
            return String::new();
        }
        let p = crate::enterprise::common::principal_of(&self.forge, principal_id).await;
        if p.kind == PrincipalKind::Agent as i32 && !p.owner_id.is_empty() {
            p.owner_id
        } else {
            principal_id.to_string()
        }
    }

    /// Cost center assignments of a scope.
    pub(crate) async fn assignments(
        tx: &mut Tx,
        scope_key: &str,
    ) -> Result<BTreeMap<String, String>> {
        let rows = tx
            .scan(&CC_ASSIGN, &Scan::scope(scope_key, SCAN_CAP))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.k1.map(|k| (r.id, k)))
            .collect())
    }

    /// Included usage and cost centers of a scope on its current plan.
    pub(crate) async fn ledger_ctx(&self, tx: &mut Tx, scope: &ResolvedScope) -> Result<LedgerCtx> {
        let (acct, _) = self.load_account(tx, scope).await?;
        self.ledger_ctx_with(tx, scope, &acct.plan_id).await
    }

    /// Included usage of `plan_id` and the cost centers of a scope.
    pub(crate) async fn ledger_ctx_with(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
        plan_id: &str,
    ) -> Result<LedgerCtx> {
        let included = self
            .plan(plan_id)
            .map(|p| {
                let mut m: BTreeMap<i32, u64> = BTreeMap::new();
                for i in p.included {
                    *m.entry(i.product).or_default() += to_micro(i.quantity.max(0.0));
                }
                m
            })
            .unwrap_or_default();
        Ok(LedgerCtx {
            included,
            assign: Self::assignments(tx, &scope.key()).await?,
        })
    }

    /// Usage rows of one org with `from_ms <= time < to_ms`, in `(time, id)` order.
    pub(crate) async fn usage_rows(
        tx: &mut Tx,
        org_id: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<Vec<(UsageRow, Row)>> {
        let mut out = Vec::new();
        let mut after: Option<Cursor> = None;
        loop {
            let q = Scan::scope(org_id, PAGE)
                .by_n1()
                .n1_min(from_ms as i64)
                .n1_max(to_ms.saturating_sub(1) as i64)
                .after(after.take());
            let rows = tx.scan_msgs::<UsageRow>(&USAGE, &q).await?;
            let n = rows.len();
            after = rows.last().map(|(_, r)| Cursor::after(r));
            out.extend(rows);
            if n < PAGE {
                return Ok(out);
            }
        }
    }

    /// The priced ledger of `scope` for month `mi` (index from [`month_of`]).
    pub(crate) async fn month_ledger(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
        ctx: &LedgerCtx,
        mi: i64,
    ) -> Result<Vec<Priced>> {
        let (from, to) = (month_start_ms(mi), month_start_ms(mi + 1));
        let mut rows: Vec<(String, String, UsageRow)> = Vec::new();
        for org in &scope.org_ids {
            for (r, row) in Self::usage_rows(tx, org, from, to).await? {
                rows.push((org.clone(), row.id, r));
            }
        }
        rows.sort_by(|a, b| (a.2.at_ms, &a.1).cmp(&(b.2.at_ms, &b.1)));
        let mut remaining = ctx.included.clone();
        let mut out = Vec::with_capacity(rows.len());
        for (org_id, id, r) in rows {
            let unit_price = self.unit_price(&r.sku);
            let gross = mul_div(r.quantity_micro, unit_price);
            let covered = match remaining.get_mut(&r.product) {
                Some(rem) => {
                    let dq = (*rem).min(r.quantity_micro);
                    *rem -= dq;
                    dq
                }
                None => 0,
            };
            let discount = mul_div(covered, unit_price).min(gross);
            let cost_center = cost_center_of(
                &ctx.assign,
                &r.repo_id,
                &r.principal_id,
                &r.attributed_to,
                &org_id,
            );
            out.push(Priced {
                id,
                org_id,
                repo_id: r.repo_id,
                principal_id: r.attributed_to,
                product: r.product,
                sku: r.sku,
                unit: r.unit,
                quantity_micro: r.quantity_micro,
                at_ms: r.at_ms,
                unit_price,
                gross,
                discount,
                net: gross - discount,
                discount_quantity_micro: covered,
                cost_center,
            });
        }
        Ok(out)
    }

    /// Meters one usage event.
    ///
    /// The record is stored in the usage table of `input.org_id`, idempotent by
    /// `idempotency_key`. The returned [`UsageOutcome`] is the state after the charge: every
    /// budget covering the record with its spend, alert thresholds crossed by this charge
    /// (each fires `billing.budget_threshold_reached` once per month) and whether a hard
    /// limit is now reached. Recording never fails because a limit is reached: the work was
    /// already done; [`Billing::enforce`] is what refuses further work.
    pub async fn record_usage(&self, cx: &Cx, input: UsageInput) -> Result<UsageOutcome> {
        if input.org_id.is_empty() {
            return Err(ForgeError::missing("org_id"));
        }
        if input.product == BillingProduct::Unspecified {
            return Err(ForgeError::missing("product"));
        }
        if input.sku.is_empty() || input.sku.len() > 200 {
            return Err(ForgeError::invalid(
                "invalid_sku",
                "sku must be 1-200 characters",
            ));
        }
        if input.unit.is_empty() || input.unit.len() > 50 {
            return Err(ForgeError::invalid(
                "invalid_unit",
                "unit must be 1-50 characters",
            ));
        }
        if !input.quantity.is_finite() || input.quantity <= 0.0 || input.quantity > 1e12 {
            return Err(ForgeError::invalid(
                "invalid_quantity",
                "quantity must be positive, finite and at most 1e12",
            ));
        }
        if input.idempotency_key.len() > 256 {
            return Err(ForgeError::invalid(
                "invalid_key",
                "idempotency key over 256 bytes",
            ));
        }
        let scopes = self.scopes_of_org(cx, &input.org_id).await?;
        let org_id = scopes
            .first()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| input.org_id.clone());
        let at_ms = if input.at_ms == 0 {
            self.db.now_ms()
        } else {
            input.at_ms
        };
        let attributed = self.attributed_to(&input.principal_id).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let existing = if input.idempotency_key.is_empty() {
            None
        } else {
            tx.scan_msgs::<UsageRow>(
                &USAGE,
                &Scan::scope(org_id.clone(), 1)
                    .key(crate::store::Key::K1(input.idempotency_key.clone())),
            )
            .await?
            .into_iter()
            .next()
        };
        let (record, duplicate) = match existing {
            Some((row, meta)) => (record_of(&org_id, &meta.id, &row), true),
            None => {
                let row = UsageRow {
                    repo_id: input.repo_id.clone(),
                    principal_id: input.principal_id.clone(),
                    attributed_to: attributed,
                    product: input.product as i32,
                    sku: input.sku.clone(),
                    quantity_micro: to_micro(input.quantity),
                    unit: input.unit.clone(),
                    at_ms,
                    key: input.idempotency_key.clone(),
                };
                let id = tx.new_id()?;
                let mut meta = Row::new(org_id.clone(), id.clone())
                    .n1(at_ms as i64)
                    .msg(&row);
                if !row.key.is_empty() {
                    meta = meta.k1(row.key.clone());
                }
                tx.insert(&USAGE, &meta).await?;
                (record_of(&org_id, &id, &row), false)
            }
        };
        let (budgets, spending_limit) = self.states_after(&mut tx, &scopes, &record).await?;
        let limit_hit = budgets
            .iter()
            .any(|b| b.hard_stop && b.spent_micro_usd >= b.amount_micro_usd)
            || spending_limit.as_ref().is_some_and(|l| l.exceeded);
        tx.commit().await?;
        Ok(UsageOutcome {
            record,
            duplicate,
            budgets,
            spending_limit,
            limit_hit,
        })
    }

    /// Returns metered usage as priced lines. A pure function of the stored usage rows, the
    /// price table in force, the plan's included usage and the cost center assignments.
    pub async fn query_usage(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        q: UsageQuery,
    ) -> Result<UsageReport> {
        let now = self.db.now_ms();
        let start = q.start_ms.unwrap_or_else(|| month_start_ms(month_of(now)));
        let end = q.end_ms.unwrap_or(now.saturating_add(1));
        if start >= end {
            return Err(ForgeError::invalid(
                "invalid_window",
                "start_at must be before end_at",
            ));
        }
        let (first, last) = (month_of(start), month_of(end - 1));
        if last - first + 1 > MAX_QUERY_MONTHS {
            return Err(ForgeError::invalid(
                "window_too_large",
                format!("a usage query spans at most {MAX_QUERY_MONTHS} months"),
            ));
        }
        let repo_id = match &q.repo {
            Some(r) if r.repo.is_some() => Some(self.repo_in_scope(cx, scope, r).await?),
            _ => None,
        };
        let org_id = if q.org.is_empty() {
            None
        } else {
            Some(self.org_in_scope(cx, scope, &q.org).await?)
        };
        let mut group = q.group_by.clone();
        if group.is_empty() {
            group = vec![BillingUsageGrouping::Day, BillingUsageGrouping::Product];
        }
        let has = |g: BillingUsageGrouping| group.contains(&g);
        let limit = match q.limit {
            0 => crate::enterprise::common::DEFAULT_LIST,
            n => n.min(MAX_LIST),
        };

        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !q.cost_center_id.is_empty() {
            get_or_404::<bee_proto::v1::CostCenter>(
                &mut tx,
                &COST_CENTERS,
                &scope.key(),
                &q.cost_center_id,
                "cost center",
            )
            .await?;
        }
        let ctx = self.ledger_ctx(&mut tx, scope).await?;
        let mut lines: BTreeMap<String, Agg> = BTreeMap::new();
        for mi in first..=last {
            for p in self.month_ledger(&mut tx, scope, &ctx, mi).await? {
                if p.at_ms < start || p.at_ms >= end {
                    continue;
                }
                if !q.products.is_empty() && !q.products.iter().any(|x| *x as i32 == p.product) {
                    continue;
                }
                if repo_id.as_ref().is_some_and(|r| *r != p.repo_id)
                    || org_id.as_ref().is_some_and(|o| *o != p.org_id)
                    || (!q.cost_center_id.is_empty() && q.cost_center_id != p.cost_center)
                {
                    continue;
                }
                let date = if has(BillingUsageGrouping::Day) {
                    day_label(p.at_ms)
                } else if has(BillingUsageGrouping::Month) {
                    month_label(month_of(p.at_ms))
                } else {
                    String::new()
                };
                let product = if has(BillingUsageGrouping::Product) {
                    p.product
                } else {
                    0
                };
                let pick = |g: BillingUsageGrouping, v: &str| {
                    if has(g) { v.to_string() } else { String::new() }
                };
                let sku = pick(BillingUsageGrouping::Sku, &p.sku);
                let repo = pick(BillingUsageGrouping::Repo, &p.repo_id);
                let org = pick(BillingUsageGrouping::Org, &p.org_id);
                let cc = pick(BillingUsageGrouping::CostCenter, &p.cost_center);
                let principal = pick(BillingUsageGrouping::Principal, &p.principal_id);
                let key = format!(
                    "{date}\u{1f}{product:03}\u{1f}{sku}\u{1f}{repo}\u{1f}{org}\u{1f}{cc}\u{1f}{principal}"
                );
                let agg = lines.entry(key).or_insert_with(|| Agg {
                    date,
                    product,
                    sku,
                    repo,
                    org,
                    cost_center: cc,
                    principal,
                    ..Agg::default()
                });
                agg.quantity_micro += u128::from(p.quantity_micro);
                agg.units.insert(p.unit.clone());
                agg.prices.insert(p.unit_price);
                agg.gross += p.gross;
                agg.discount += p.discount;
                agg.net += p.net;
            }
        }
        let total_gross = lines.values().map(|a| a.gross).sum();
        let total_net = lines.values().map(|a| a.net).sum();
        let mut items: Vec<(String, BillingUsageLineItem)> = Vec::new();
        let mut next = String::new();
        for (key, agg) in lines.iter() {
            if !q.after.is_empty() && key.as_str() <= q.after.as_str() {
                continue;
            }
            if items.len() == limit {
                next = items.last().map(|(k, _)| k.clone()).unwrap_or_default();
                break;
            }
            items.push((key.clone(), agg.to_item()));
        }
        let line_items = items.into_iter().map(|(_, i)| i).collect();
        Ok(UsageReport {
            line_items,
            total_gross_micro_usd: total_gross,
            total_net_micro_usd: total_net,
            data_complete_until_ms: now.saturating_sub(self.cfg.usage_lag_secs * 1000),
            next_token: next,
        })
    }
}

#[derive(Default)]
struct Agg {
    date: String,
    product: i32,
    sku: String,
    repo: String,
    org: String,
    cost_center: String,
    principal: String,
    quantity_micro: u128,
    units: std::collections::BTreeSet<String>,
    prices: std::collections::BTreeSet<u64>,
    gross: u64,
    discount: u64,
    net: u64,
}

impl Agg {
    fn to_item(&self) -> BillingUsageLineItem {
        // Quantities only add up within one unit; a line mixing units reports no quantity.
        let (quantity, unit) = if self.units.len() == 1 {
            (
                self.quantity_micro as f64 / 1_000_000.0,
                self.units.iter().next().cloned().unwrap_or_default(),
            )
        } else {
            (0.0, String::new())
        };
        let unit_price = if self.prices.len() == 1 {
            self.prices.iter().next().copied().unwrap_or(0)
        } else {
            0
        };
        BillingUsageLineItem {
            date: self.date.clone(),
            product: self.product,
            sku: self.sku.clone(),
            repo: (!self.repo.is_empty()).then(|| RepoRef {
                repo: Some(repo_ref::Repo::Id(self.repo.clone())),
            }),
            org: self.org.clone(),
            cost_center_id: self.cost_center.clone(),
            principal_id: self.principal.clone(),
            quantity,
            unit,
            unit_price_micro_usd: unit_price,
            gross_micro_usd: self.gross,
            discount_micro_usd: self.discount,
            net_micro_usd: self.net,
        }
    }
}

/// Feeds `usage.recorded` events into [`Billing::record_usage`].
///
/// Register with `forge.events.add_listener(ingestor)`. The event's `org_id` names the org,
/// its `time` is the usage time, and `payload_json` is
/// `{"product","sku","quantity","unit","principal_id","repo_id","key"}`: `product` is a
/// product name such as `actions_minutes` (see [`parse_product_name`]), `quantity` a number,
/// `principal_id` and `repo_id` default to the event's own, and `key` (the idempotency key)
/// defaults to the event's log position, so redelivery of one event never double counts.
pub struct UsageIngestor {
    billing: Arc<Billing>,
    ingested: AtomicU64,
    failed: AtomicU64,
}

impl UsageIngestor {
    /// An ingestor over `billing`.
    pub fn new(billing: Arc<Billing>) -> Arc<UsageIngestor> {
        Arc::new(UsageIngestor {
            billing,
            ingested: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }

    /// Events recorded so far (duplicates included).
    pub fn ingested(&self) -> u64 {
        self.ingested.load(Ordering::Relaxed)
    }

    /// Events that could not be recorded (malformed payload, unknown org).
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Meters one event; `Ok(None)` for events of other kinds.
    pub async fn ingest(&self, event: &Event) -> Result<Option<UsageOutcome>> {
        if event.kind != "usage.recorded" {
            return Ok(None);
        }
        let bad = |m: &str| ForgeError::invalid("bad_usage_event", m.to_string());
        let v: serde_json::Value =
            serde_json::from_str(&event.payload_json).map_err(|_| bad("payload is not JSON"))?;
        let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let product = parse_product_name(&text("product")).ok_or_else(|| bad("unknown product"))?;
        let quantity = match v.get("quantity") {
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            Some(serde_json::Value::String(s)) => s.parse::<f64>().ok(),
            _ => None,
        }
        .ok_or_else(|| bad("quantity is missing"))?;
        let or = |a: String, b: &str| if a.is_empty() { b.to_string() } else { a };
        let key = if text("key").is_empty() {
            let log = logs_of(event).into_iter().next().unwrap_or_default();
            format!("event:{log}:{}", event.seq)
        } else {
            text("key")
        };
        let input = UsageInput {
            org_id: event.org_id.clone(),
            repo_id: or(text("repo_id"), &event.repo_id),
            principal_id: or(text("principal_id"), &event.principal),
            product,
            sku: text("sku"),
            quantity,
            unit: text("unit"),
            idempotency_key: key,
            at_ms: event.time.as_ref().map(ts_ms).unwrap_or(0),
        };
        self.billing
            .record_usage(&Cx::system(), input)
            .await
            .map(Some)
    }
}

#[async_trait]
impl EventListener for UsageIngestor {
    async fn on_event(&self, event: &Event) {
        match self.ingest(event).await {
            Ok(Some(_)) => {
                self.ingested.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => {}
            Err(_) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
