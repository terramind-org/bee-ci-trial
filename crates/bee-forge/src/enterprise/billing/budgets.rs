//! Budgets, alert thresholds, hard stops and spending limits.
//!
//! A budget's *spent* is the net metered charge of the current UTC month (after included
//! usage) of the usage it covers: the whole account, one org, one repo or one cost center,
//! optionally for one product. Alert thresholds fire `billing.budget_threshold_reached` once
//! per threshold per month, when usage is recorded. A `hard_stop` budget makes
//! [`Billing::enforce`] and [`SpendGate::check_spend`] answer `QUOTA_EXCEEDED` once its amount
//! is spent or would be passed by the charge. When no budget covers a charge,
//! [`BillingConfig::default_spending_limit_micro_usd`](super::BillingConfig) applies to the
//! paying account's month in the same way.

use async_trait::async_trait;
use bee_proto::v1::billing_budget_target::Target;
use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::{
    BillingBudget, BillingBudgetTarget, BillingProduct, CreateBillingBudgetRequest, ErrorReason,
    LimitExceeded, SubjectKind, UpdateBillingBudgetRequest,
};
use serde_json::json;

use super::records::AlertRow;
use super::time::{month_label, month_of, yyyymm};
use super::usage::{Priced, to_micro};
use super::{
    ALERTS, BUDGETS, Billing, COST_CENTERS, COUNTERS, Change, SURFACE, UsageRecord, audit_change,
    event_org, record_change,
};
use crate::cx::Cx;
use crate::enterprise::common::{
    ResolvedScope, SCAN_CAP, SpendGate, emit, get_or_404, json as to_json, scan_all,
};
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Scan, Tx};

/// Most budgets one account holds.
pub const MAX_BUDGETS: usize = 500;
/// Most alert recipients on one budget.
pub const MAX_RECIPIENTS: usize = 50;
const MAX_THRESHOLD_PERCENT: u32 = 1000;

/// A budget's standing after a charge.
#[derive(Clone, Debug, PartialEq)]
pub struct BudgetState {
    /// Budget id.
    pub budget_id: String,
    /// Budget name.
    pub name: String,
    /// Scope key of the account owning the budget.
    pub scope_key: String,
    /// Product filter (`Unspecified` = all).
    pub product: BillingProduct,
    /// Monthly amount, micro-USD.
    pub amount_micro_usd: u64,
    /// Spent this month, micro-USD.
    pub spent_micro_usd: u64,
    /// Whether the budget blocks further usage at its amount.
    pub hard_stop: bool,
    /// Alert thresholds (percent) newly fired by this charge.
    pub fired_thresholds: Vec<u32>,
}

/// The default spending limit's standing after a charge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitState {
    /// Scope key of the account the limit applies to.
    pub scope_key: String,
    /// The limit, micro-USD per month.
    pub limit_micro_usd: u64,
    /// Spent this month, micro-USD.
    pub spent_micro_usd: u64,
    /// Whether the limit is reached.
    pub exceeded: bool,
}

/// Whether a budget covers usage of `product` by (`org`, `repo`, `cost_center`).
fn covers(b: &BillingBudget, org: &str, repo: &str, cost_center: &str, product: i32) -> bool {
    if b.product != 0 && b.product != product {
        return false;
    }
    match b.target.as_ref().and_then(|t| t.target.as_ref()) {
        None | Some(Target::Account(_)) => true,
        Some(Target::Org(o)) => o == org,
        Some(Target::Repo(r)) => matches!(
            &r.repo,
            Some(bee_proto::v1::repo_ref::Repo::Id(id)) if !repo.is_empty() && id == repo
        ),
        Some(Target::CostCenterId(c)) => !cost_center.is_empty() && c == cost_center,
    }
}

fn spent_by(b: &BillingBudget, ledger: &[Priced]) -> u64 {
    ledger
        .iter()
        .filter(|p| covers(b, &p.org_id, &p.repo_id, &p.cost_center, p.product))
        .map(|p| p.net)
        .sum()
}

fn exhausted(subcode: &str, what: &str, amount: u64, spent: u64, add: u64) -> ForgeError {
    ForgeError::new(
        ErrorReason::QuotaExceeded,
        format!("{what} of {amount} micro-USD: {spent} spent, {add} more requested"),
    )
    .with_subcode(subcode)
    .with_payload(Payload::LimitExceeded(LimitExceeded {
        limit: what.to_string(),
        maximum: amount,
        actual: spent.saturating_add(add),
    }))
}

impl Billing {
    fn clean_thresholds(p: &[u32]) -> Result<Vec<u32>> {
        let mut v = p.to_vec();
        if v.iter().any(|t| *t == 0 || *t > MAX_THRESHOLD_PERCENT) {
            return Err(ForgeError::invalid(
                "invalid_threshold",
                format!("alert thresholds are percents from 1 to {MAX_THRESHOLD_PERCENT}"),
            ));
        }
        v.sort_unstable();
        v.dedup();
        Ok(v)
    }

    fn clean_recipients(r: &[String]) -> Result<Vec<String>> {
        if r.len() > MAX_RECIPIENTS || r.iter().any(|x| x.is_empty()) {
            return Err(ForgeError::invalid(
                "invalid_recipients",
                format!("at most {MAX_RECIPIENTS} non-empty recipient ids"),
            ));
        }
        let mut v = r.to_vec();
        v.sort();
        v.dedup();
        Ok(v)
    }

    /// Resolves org names and repo references of a target to ids and checks they belong to
    /// the scope. The cost center is checked inside the transaction.
    async fn normalize_target(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        t: &Option<BillingBudgetTarget>,
    ) -> Result<BillingBudgetTarget> {
        let target = match t.as_ref().and_then(|t| t.target.as_ref()) {
            None | Some(Target::Account(_)) => Target::Account(true),
            Some(Target::Org(o)) => Target::Org(self.org_in_scope(cx, scope, o).await?),
            Some(Target::Repo(r)) => Target::Repo(bee_proto::v1::RepoRef {
                repo: Some(bee_proto::v1::repo_ref::Repo::Id(
                    self.repo_in_scope(cx, scope, r).await?,
                )),
            }),
            Some(Target::CostCenterId(c)) => {
                if c.is_empty() {
                    return Err(ForgeError::missing("target.cost_center_id"));
                }
                Target::CostCenterId(c.clone())
            }
        };
        Ok(BillingBudgetTarget {
            target: Some(target),
        })
    }

    async fn check_target(
        tx: &mut Tx,
        scope: &ResolvedScope,
        t: &BillingBudgetTarget,
    ) -> Result<()> {
        if let Some(Target::CostCenterId(c)) = &t.target {
            get_or_404::<bee_proto::v1::CostCenter>(
                tx,
                &COST_CENTERS,
                &scope.key(),
                c,
                "cost center",
            )
            .await?;
        }
        Ok(())
    }

    /// Current-month net spend of every budget in `budgets`, in the same order.
    async fn spent_now(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
        budgets: &[BillingBudget],
    ) -> Result<Vec<u64>> {
        if budgets.is_empty() {
            return Ok(Vec::new());
        }
        let ctx = self.ledger_ctx(tx, scope).await?;
        let ledger = self
            .month_ledger(tx, scope, &ctx, month_of(tx.now_ms()))
            .await?;
        Ok(budgets.iter().map(|b| spent_by(b, &ledger)).collect())
    }

    fn view(scope: &ResolvedScope, mut b: BillingBudget, spent: u64) -> BillingBudget {
        b.scope = Some(scope.to_proto());
        b.spent_micro_usd = spent;
        b
    }

    /// Creates a budget.
    pub async fn create_budget(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        req: CreateBillingBudgetRequest,
    ) -> Result<BillingBudget> {
        let name = req.name.trim().to_string();
        if name.is_empty() || name.len() > 200 {
            return Err(ForgeError::invalid(
                "invalid_name",
                "name must be 1-200 characters",
            ));
        }
        if BillingProduct::try_from(req.product).is_err() {
            return Err(ForgeError::invalid("invalid_product", "unknown product"));
        }
        let thresholds = Self::clean_thresholds(&req.alert_threshold_percents)?;
        let recipients = Self::clean_recipients(&req.alert_recipient_ids)?;
        let target = self.normalize_target(cx, scope, &req.target).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::check_target(&mut tx, scope, &target).await?;
        let key = scope.key();
        tx.add_n1(&COUNTERS, &key, "budget_scope_cap", 0).await?;
        if tx.count(&BUDGETS, &key, None).await? as usize >= MAX_BUDGETS {
            return Err(ForgeError::too_large(
                "budgets",
                MAX_BUDGETS as u64,
                MAX_BUDGETS as u64 + 1,
            ));
        }
        let now = ts(tx.now_ms());
        let b = BillingBudget {
            id: tx.new_id()?,
            scope: None,
            name,
            product: req.product,
            target: Some(target),
            monthly_amount_micro_usd: req.monthly_amount_micro_usd,
            alert_threshold_percents: thresholds,
            alert_recipient_ids: recipients,
            hard_stop: req.hard_stop,
            spent_micro_usd: 0,
            created_at: Some(now),
            updated_at: Some(now),
        };
        tx.insert(&BUDGETS, &Row::new(key, b.id.clone()).msg(&b))
            .await?;
        let out = Self::view(scope, b, 0);
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.budget_created",
                action: "billing.budget.create",
                target: &out.id,
                before: None,
                resource: &to_json(&out),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// One budget with this month's spend.
    pub async fn get_budget(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
    ) -> Result<BillingBudget> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (b, _) =
            get_or_404::<BillingBudget>(&mut tx, &BUDGETS, &scope.key(), id, "budget").await?;
        let spent = self
            .spent_now(&mut tx, scope, std::slice::from_ref(&b))
            .await?;
        Ok(Self::view(scope, b, spent[0]))
    }

    /// Changes a budget; unset fields are unchanged.
    pub async fn update_budget(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        req: UpdateBillingBudgetRequest,
    ) -> Result<BillingBudget> {
        let thresholds = if req.replace_alerts {
            Some(Self::clean_thresholds(&req.alert_threshold_percents)?)
        } else {
            None
        };
        let recipients = if req.replace_alerts {
            Some(Self::clean_recipients(&req.alert_recipient_ids)?)
        } else {
            None
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut b, row) =
            get_or_404::<BillingBudget>(&mut tx, &BUDGETS, &scope.key(), &req.budget_id, "budget")
                .await?;
        let before = to_json(&Self::view(scope, b.clone(), 0));
        if let Some(n) = req.name {
            let n = n.trim().to_string();
            if n.is_empty() || n.len() > 200 {
                return Err(ForgeError::invalid(
                    "invalid_name",
                    "name must be 1-200 characters",
                ));
            }
            b.name = n;
        }
        if let Some(a) = req.monthly_amount_micro_usd {
            b.monthly_amount_micro_usd = a;
        }
        if let Some(t) = thresholds {
            b.alert_threshold_percents = t;
        }
        if let Some(r) = recipients {
            b.alert_recipient_ids = r;
        }
        if let Some(h) = req.hard_stop {
            b.hard_stop = h;
        }
        b.updated_at = Some(ts(tx.now_ms()));
        tx.update(&BUDGETS, &row.msg(&b)).await?;
        let spent = self
            .spent_now(&mut tx, scope, std::slice::from_ref(&b))
            .await?;
        let out = Self::view(scope, b, spent[0]);
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.budget_updated",
                action: "billing.budget.update",
                target: &out.id,
                before: Some(&before),
                resource: &to_json(&out),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Deletes a budget and its alert markers.
    pub async fn delete_budget(&self, cx: &Cx, scope: &ResolvedScope, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (b, _) =
            get_or_404::<BillingBudget>(&mut tx, &BUDGETS, &scope.key(), id, "budget").await?;
        let before = to_json(&Self::view(scope, b, 0));
        tx.delete(&BUDGETS, &scope.key(), id).await?;
        for r in tx.scan(&ALERTS, &Scan::scope(id, SCAN_CAP)).await? {
            tx.delete(&ALERTS, id, &r.id).await?;
        }
        emit(
            &mut tx,
            "billing.budget_deleted",
            &event_org(scope),
            Some((SubjectKind::Unspecified, id)),
            json!({ "scope": scope.key(), "id": id }).to_string(),
        )
        .await?;
        audit_change(
            &mut tx,
            scope,
            "billing.budget.delete",
            id,
            Some(&before),
            "{}",
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Budgets of an account ordered by id, optionally of one product, with this month's
    /// spend; returns the page and the next token.
    pub async fn list_budgets(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        product: BillingProduct,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<BillingBudget>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all = scan_all::<BillingBudget>(&mut tx, &BUDGETS, &scope.key(), None).await?;
        let mut budgets: Vec<BillingBudget> = all
            .into_iter()
            .map(|(b, _)| b)
            .filter(|b| product == BillingProduct::Unspecified || b.product == product as i32)
            .filter(|b| after.is_empty() || b.id.as_str() > after)
            .collect();
        let limit = limit.clamp(1, crate::enterprise::common::MAX_LIST);
        let next = if budgets.len() > limit {
            budgets.truncate(limit);
            budgets.last().map(|b| b.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        let spent = self.spent_now(&mut tx, scope, &budgets).await?;
        Ok((
            budgets
                .into_iter()
                .zip(spent)
                .map(|(b, s)| Self::view(scope, b, s))
                .collect(),
            next,
        ))
    }

    /// Fires the alert thresholds a budget's spend has reached and no alert marker of this
    /// month records yet; returns the thresholds fired.
    async fn fire_alerts(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
        b: &BillingBudget,
        mi: i64,
        spent: u64,
        org_id: &str,
    ) -> Result<Vec<u32>> {
        let mut fired = Vec::new();
        let mut thresholds = b.alert_threshold_percents.clone();
        thresholds.sort_unstable();
        thresholds.dedup();
        for t in thresholds {
            let reached = spent > 0
                && u128::from(spent) * 100
                    >= u128::from(b.monthly_amount_micro_usd) * u128::from(t);
            if !reached {
                continue;
            }
            let id = format!("{}:{t}", yyyymm(mi));
            if tx.get(&ALERTS, &b.id, &id).await?.is_some() {
                continue;
            }
            let now = tx.now_ms();
            tx.insert(
                &ALERTS,
                &Row::new(b.id.clone(), id).msg(&AlertRow {
                    fired_ms: now,
                    spent_micro_usd: spent,
                }),
            )
            .await?;
            let emails = self.alert_emails(tx, scope).await?;
            let payload = json!({
                "scope": scope.key(),
                "budget_id": b.id,
                "name": b.name,
                "month": month_label(mi),
                "threshold_percent": t,
                "spent_micro_usd": spent,
                "amount_micro_usd": b.monthly_amount_micro_usd,
                "hard_stop": b.hard_stop,
                "recipients": b.alert_recipient_ids,
                "emails": emails,
            })
            .to_string();
            emit(
                tx,
                "billing.budget_threshold_reached",
                org_id,
                Some((SubjectKind::Unspecified, &b.id)),
                payload.clone(),
            )
            .await?;
            audit_change(tx, scope, "billing.budget.alert", &b.id, None, &payload).await?;
            fired.push(t);
        }
        Ok(fired)
    }

    /// Every budget covering `rec` with its spend, firing the thresholds it crossed; when no
    /// budget covers it, the default spending limit's standing.
    pub(crate) async fn states_after(
        &self,
        tx: &mut Tx,
        scopes: &[ResolvedScope],
        rec: &UsageRecord,
    ) -> Result<(Vec<BudgetState>, Option<LimitState>)> {
        let mi = month_of(rec.at_ms);
        let mut states = Vec::new();
        for scope in scopes {
            let budgets = scan_all::<BillingBudget>(tx, &BUDGETS, &scope.key(), None).await?;
            if budgets.is_empty() {
                continue;
            }
            let ctx = self.ledger_ctx(tx, scope).await?;
            let ledger = self.month_ledger(tx, scope, &ctx, mi).await?;
            let Some(me) = ledger.iter().find(|p| p.id == rec.id) else {
                continue;
            };
            for (b, _) in budgets {
                if !covers(&b, &me.org_id, &me.repo_id, &me.cost_center, me.product) {
                    continue;
                }
                let spent = spent_by(&b, &ledger);
                let fired = self
                    .fire_alerts(tx, scope, &b, mi, spent, &rec.org_id)
                    .await?;
                states.push(BudgetState {
                    budget_id: b.id.clone(),
                    name: b.name.clone(),
                    scope_key: scope.key(),
                    product: BillingProduct::try_from(b.product)
                        .unwrap_or(BillingProduct::Unspecified),
                    amount_micro_usd: b.monthly_amount_micro_usd,
                    spent_micro_usd: spent,
                    hard_stop: b.hard_stop,
                    fired_thresholds: fired,
                });
            }
        }
        let limit = self.cfg.default_spending_limit_micro_usd;
        let mut spending = None;
        if states.is_empty()
            && limit > 0
            && let Some(scope) = scopes.last()
        {
            let ctx = self.ledger_ctx(tx, scope).await?;
            let ledger = self.month_ledger(tx, scope, &ctx, mi).await?;
            let spent: u64 = ledger.iter().map(|p| p.net).sum();
            spending = Some(LimitState {
                scope_key: scope.key(),
                limit_micro_usd: limit,
                spent_micro_usd: spent,
                exceeded: spent >= limit,
            });
        }
        Ok((states, spending))
    }

    /// Refuses a charge of `add_micro_usd` for `product` by `principal_id` in `org_id` and
    /// `repo_id` when a hard-stop budget covering it is spent or would be passed, or, with no
    /// budget covering it, when the default spending limit would be.
    pub(crate) async fn gate(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        principal_id: &str,
        product: BillingProduct,
        add_micro_usd: u64,
    ) -> Result<()> {
        if org_id.is_empty() {
            return Err(ForgeError::missing("org_id"));
        }
        let scopes = self.scopes_of_org(cx, org_id).await?;
        let org = scopes.first().map(|s| s.id.clone()).unwrap_or_default();
        let attributed = self.attributed_to(principal_id).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mi = month_of(tx.now_ms());
        let mut any = false;
        for scope in &scopes {
            let budgets = scan_all::<BillingBudget>(&mut tx, &BUDGETS, &scope.key(), None).await?;
            if budgets.is_empty() {
                continue;
            }
            let ctx = self.ledger_ctx(&mut tx, scope).await?;
            let cc =
                super::usage::cost_center_of(&ctx.assign, repo_id, principal_id, &attributed, &org);
            let mut ledger: Option<Vec<Priced>> = None;
            for (b, _) in budgets {
                if !covers(&b, &org, repo_id, &cc, product as i32) {
                    continue;
                }
                any = true;
                if !b.hard_stop {
                    continue;
                }
                if ledger.is_none() {
                    ledger = Some(self.month_ledger(&mut tx, scope, &ctx, mi).await?);
                }
                let spent = spent_by(&b, ledger.as_deref().unwrap_or(&[]));
                let amount = b.monthly_amount_micro_usd;
                if spent >= amount || spent.saturating_add(add_micro_usd) > amount {
                    return Err(exhausted(
                        "budget_exhausted",
                        &format!("budget {} ({})", b.name, b.id),
                        amount,
                        spent,
                        add_micro_usd,
                    ));
                }
            }
        }
        let limit = self.cfg.default_spending_limit_micro_usd;
        if !any
            && limit > 0
            && let Some(scope) = scopes.last()
        {
            let ctx = self.ledger_ctx(&mut tx, scope).await?;
            let spent: u64 = self
                .month_ledger(&mut tx, scope, &ctx, mi)
                .await?
                .iter()
                .map(|p| p.net)
                .sum();
            if spent >= limit || spent.saturating_add(add_micro_usd) > limit {
                return Err(exhausted(
                    "spending_limit",
                    "spending limit",
                    limit,
                    spent,
                    add_micro_usd,
                ));
            }
        }
        Ok(())
    }

    /// Admission check the server calls before metered work (CI minutes, storage, agent
    /// runs): prices `quantity` of `sku` from the price table and fails with
    /// `QUOTA_EXCEEDED` when a hard-stop budget or the spending limit covering the acting
    /// principal, repo and org cannot take the charge.
    pub async fn enforce(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        product: BillingProduct,
        sku: &str,
        quantity: f64,
    ) -> Result<()> {
        if !quantity.is_finite() || quantity < 0.0 {
            return Err(ForgeError::invalid(
                "invalid_quantity",
                "quantity must be finite and not negative",
            ));
        }
        let add =
            (u128::from(to_micro(quantity)) * u128::from(self.unit_price(sku)) / 1_000_000) as u64;
        self.gate(cx, org_id, repo_id, &cx.actor_id, product, add)
            .await
    }
}

#[async_trait]
impl SpendGate for Billing {
    async fn check_spend(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        product: BillingProduct,
        add_micro_usd: u64,
    ) -> Result<()> {
        self.gate(cx, org_id, repo_id, &cx.actor_id, product, add_micro_usd)
            .await
    }
}

impl Billing {
    /// Addresses budget alerts also go to: the primary and additional billing emails when the
    /// account's settings send usage alerts.
    async fn alert_emails(&self, tx: &mut Tx, scope: &ResolvedScope) -> Result<Vec<String>> {
        let s = self.load_email(tx, scope).await?;
        if !s.send_usage_alerts {
            return Ok(Vec::new());
        }
        let mut v: Vec<String> = std::iter::once(s.primary_email)
            .chain(s.additional_emails)
            .filter(|e| !e.is_empty())
            .collect();
        v.sort();
        v.dedup();
        Ok(v)
    }
}
