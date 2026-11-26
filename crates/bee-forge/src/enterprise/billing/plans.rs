//! Plans, seats and the account subscription.

use std::collections::BTreeMap;

use bee_proto::v1::{
    BillingAccountPlan, BillingCycle, BillingIncludedUsage, BillingPlan, BillingProduct,
    BillingSeat, Principal, Role,
};

use super::records::AccountRow;
use super::time::{month_of, month_start_ms};
use super::{ACCOUNTS, Billing, Change, SURFACE, USAGE, record_change};
use crate::cx::Cx;
use crate::enterprise::common::{ResolvedScope, ScopeKind, json};
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Scan, Tx};

/// Largest number of seats one account can buy.
pub const MAX_SEATS: u32 = 1_000_000;

fn included(product: BillingProduct, quantity: f64, unit: &str) -> BillingIncludedUsage {
    BillingIncludedUsage {
        product: product as i32,
        quantity,
        unit: unit.into(),
    }
}

fn features(f: &[&str]) -> Vec<String> {
    f.iter().map(|s| s.to_string()).collect()
}

/// The Team plan's features, shared by the programs that discount it.
const TEAM_FEATURES: &[&str] = &[
    "repos",
    "issues",
    "change_requests",
    "protected_refs",
    "code_owners",
    "teams",
    "email_support",
];

/// The built-in plan catalog: `free`, `team`, `enterprise` (contact sales), and the
/// `education` and `nonprofit` programs: Team's features and included usage at a discounted
/// seat price (free, and half), assigned by a program administrator once eligibility is
/// established, never self-serve.
pub fn builtin_plans() -> Vec<BillingPlan> {
    use BillingProduct as P;
    let team_included = || {
        vec![
            included(P::ActionsMinutes, 3_000.0, "minute"),
            included(P::StorageGb, 2.0, "GB-month"),
            included(P::AgentTokens, 1_000_000.0, "token"),
            included(P::PackagesTransfer, 10.0, "GB"),
            included(P::Lfs, 2.0, "GB"),
        ]
    };
    vec![
        BillingPlan {
            id: "free".into(),
            name: "Free".into(),
            description: "Public and private repos for individuals and small teams".into(),
            seat_monthly_price_micro_usd: 0,
            seat_yearly_price_micro_usd: 0,
            included: vec![
                included(P::ActionsMinutes, 2_000.0, "minute"),
                included(P::StorageGb, 1.0, "GB-month"),
                included(P::AgentTokens, 100_000.0, "token"),
                included(P::PackagesTransfer, 1.0, "GB"),
            ],
            features: features(&["repos", "issues", "change_requests", "community_support"]),
            self_serve: true,
        },
        BillingPlan {
            id: "team".into(),
            name: "Team".into(),
            description: "Protected refs, code owners and more included usage".into(),
            seat_monthly_price_micro_usd: 4_000_000,
            seat_yearly_price_micro_usd: 40_000_000,
            included: team_included(),
            features: features(TEAM_FEATURES),
            self_serve: true,
        },
        BillingPlan {
            id: "enterprise".into(),
            name: "Enterprise".into(),
            description: "SSO, SCIM, audit streaming, IP allow lists and the largest allowances"
                .into(),
            seat_monthly_price_micro_usd: 21_000_000,
            seat_yearly_price_micro_usd: 210_000_000,
            included: vec![
                included(P::ActionsMinutes, 50_000.0, "minute"),
                included(P::StorageGb, 50.0, "GB-month"),
                included(P::AgentTokens, 50_000_000.0, "token"),
                included(P::PackagesTransfer, 100.0, "GB"),
                included(P::Lfs, 50.0, "GB"),
            ],
            features: features(&[
                "repos",
                "issues",
                "change_requests",
                "protected_refs",
                "code_owners",
                "teams",
                "sso",
                "scim",
                "audit_streaming",
                "ip_allow_lists",
                "enterprise_accounts",
                "priority_support",
            ]),
            self_serve: false,
        },
        BillingPlan {
            id: "education".into(),
            name: "Education".into(),
            description: "Team for students, teachers and schools: a 100% discount".into(),
            seat_monthly_price_micro_usd: 0,
            seat_yearly_price_micro_usd: 0,
            included: team_included(),
            features: features(TEAM_FEATURES),
            self_serve: false,
        },
        BillingPlan {
            id: "nonprofit".into(),
            name: "Nonprofit".into(),
            description: "Team for registered nonprofits: a 50% discount".into(),
            seat_monthly_price_micro_usd: 2_000_000,
            seat_yearly_price_micro_usd: 20_000_000,
            included: team_included(),
            features: features(TEAM_FEATURES),
            self_serve: false,
        },
    ]
}

/// The built-in price table: SKU to micro-USD per unit.
pub fn default_prices() -> BTreeMap<String, u64> {
    [
        ("actions_linux_2core", 8_000),
        ("actions_linux_4core", 16_000),
        ("actions_linux_8core", 32_000),
        ("actions_windows_2core", 16_000),
        ("actions_macos_3core", 80_000),
        ("storage_gb_month", 250_000),
        ("workspaces_2core_hour", 90_000),
        ("workspaces_4core_hour", 180_000),
        ("agent_tokens_input", 3),
        ("agent_tokens_output", 15),
        ("packages_transfer_gb", 500_000),
        ("lfs_storage_gb_month", 70_000),
        ("lfs_bandwidth_gb", 70_000),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// End (exclusive) of the term starting at `start_ms`.
pub(crate) fn term_end_ms(start_ms: u64, cycle: i32) -> u64 {
    let months = if cycle == BillingCycle::Yearly as i32 {
        12
    } else {
        1
    };
    month_start_ms(month_of(start_ms) + months)
}

/// A requested change of plan, seats or cycle.
#[derive(Clone, Debug, Default)]
pub struct PlanChange {
    /// The plan to be on.
    pub plan_id: String,
    /// Seats to buy; 0 keeps the current count.
    pub seats: u32,
    /// Billing cycle; unspecified keeps the current one.
    pub cycle: BillingCycle,
}

/// A principal consuming a paid seat.
#[derive(Clone, Debug, PartialEq)]
pub struct SeatInfo {
    /// The principal.
    pub principal: Principal,
    /// Org id through which the seat is consumed (the smallest org id when in several).
    pub org_id: String,
    /// Whether the principal is an outside collaborator rather than a member.
    pub outside_collaborator: bool,
    /// Earliest membership time across the scope's orgs, ms (0 when unknown).
    pub consumed_since_ms: u64,
}

impl Billing {
    /// The plan catalog in force.
    pub fn catalog(&self) -> Vec<BillingPlan> {
        if self.cfg.plans.is_empty() {
            builtin_plans()
        } else {
            self.cfg.plans.clone()
        }
    }

    /// One plan of the catalog.
    pub fn plan(&self, id: &str) -> Option<BillingPlan> {
        self.catalog().into_iter().find(|p| p.id == id)
    }

    fn default_plan_id(&self) -> String {
        let cat = self.catalog();
        cat.iter()
            .find(|p| p.id == "free")
            .or_else(|| cat.iter().find(|p| p.seat_monthly_price_micro_usd == 0))
            .or(cat.first())
            .map(|p| p.id.clone())
            .unwrap_or_default()
    }

    /// Orders plans: by monthly seat price, then catalog position.
    fn rank(&self, id: &str) -> (u64, usize) {
        self.catalog()
            .iter()
            .enumerate()
            .find(|(_, p)| p.id == id)
            .map(|(i, p)| (p.seat_monthly_price_micro_usd, i))
            .unwrap_or((0, 0))
    }

    /// The plans an account of this kind may choose. Enterprise accounts are not offered the
    /// free plan.
    pub fn list_plans(&self, scope: &ResolvedScope) -> Vec<BillingPlan> {
        self.catalog()
            .into_iter()
            .filter(|p| {
                !(scope.kind == ScopeKind::Enterprise && p.seat_monthly_price_micro_usd == 0)
            })
            .collect()
    }

    /// Reads an account's plan row; an account that never chose a plan is on the default plan
    /// from the month of its first usage.
    pub(crate) async fn load_account(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
    ) -> Result<(AccountRow, Option<Row>)> {
        if let Some((a, row)) = tx
            .get_msg::<AccountRow>(&ACCOUNTS, &scope.key(), "plan")
            .await?
        {
            return Ok((a, Some(row)));
        }
        let now = tx.now_ms();
        let mut first: Option<u64> = None;
        for org in &scope.org_ids {
            let rows = tx
                .scan(&USAGE, &Scan::scope(org.clone(), 1).by_n1())
                .await?;
            if let Some(n) = rows.first().and_then(|r| r.n1) {
                first = Some(first.map_or(n as u64, |f| f.min(n as u64)));
            }
        }
        let start = month_start_ms(month_of(first.unwrap_or(now).min(now)));
        Ok((
            AccountRow {
                plan_id: self.default_plan_id(),
                cycle: BillingCycle::Monthly as i32,
                seats_purchased: 0,
                term_start_ms: start,
                pending_plan_id: String::new(),
                pending_seats: 0,
                updated_ms: 0,
            },
            None,
        ))
    }

    /// Writes an account row (insert when new, compare-and-swap otherwise).
    pub(crate) async fn save_account(
        tx: &mut Tx,
        scope: &ResolvedScope,
        acct: &AccountRow,
        row: Option<Row>,
    ) -> Result<()> {
        match row {
            Some(r) => {
                tx.update(&ACCOUNTS, &r.msg(acct)).await?;
            }
            None => {
                tx.insert(&ACCOUNTS, &Row::new(scope.key(), "plan").msg(acct))
                    .await?;
            }
        }
        Ok(())
    }

    /// Everyone consuming a seat in `scope`: distinct org members (except billing managers)
    /// and outside collaborators across the scope's orgs, ordered by principal id.
    pub async fn seats(&self, cx: &Cx, scope: &ResolvedScope) -> Result<Vec<SeatInfo>> {
        let mut by: BTreeMap<String, SeatInfo> = BTreeMap::new();
        let mut orgs = scope.org_ids.clone();
        orgs.sort();
        for org in &orgs {
            let mut after = String::new();
            loop {
                let (members, next) = self.forge.orgs.members(cx, org, &after, 500).await?;
                for m in members {
                    if m.role == Role::BillingManager as i32 {
                        continue;
                    }
                    let Some(p) = m.principal else { continue };
                    let since = m.joined_at.as_ref().map(crate::ids::ts_ms).unwrap_or(0);
                    match by.get_mut(&p.id) {
                        Some(s) => {
                            s.outside_collaborator = false;
                            if since != 0
                                && (s.consumed_since_ms == 0 || since < s.consumed_since_ms)
                            {
                                s.consumed_since_ms = since;
                            }
                        }
                        None => {
                            by.insert(
                                p.id.clone(),
                                SeatInfo {
                                    principal: p,
                                    org_id: org.clone(),
                                    outside_collaborator: false,
                                    consumed_since_ms: since,
                                },
                            );
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                after = next;
            }
        }
        for org in &orgs {
            for c in self.forge.orgs.outside_collaborators(cx, org).await? {
                let Some(p) = c.principal else { continue };
                by.entry(p.id.clone()).or_insert(SeatInfo {
                    principal: p,
                    org_id: org.clone(),
                    outside_collaborator: true,
                    consumed_since_ms: 0,
                });
            }
        }
        Ok(by.into_values().collect())
    }

    /// An account's plan, seats and cycle.
    pub async fn account_plan(&self, cx: &Cx, scope: &ResolvedScope) -> Result<BillingAccountPlan> {
        let consumed = self.seats(cx, scope).await?.len() as u32;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (acct, _) = self.load_account(&mut tx, scope).await?;
        Ok(self.account_view(scope, &acct, consumed))
    }

    pub(crate) fn account_view(
        &self,
        scope: &ResolvedScope,
        acct: &AccountRow,
        consumed: u32,
    ) -> BillingAccountPlan {
        let plan = self.plan(&acct.plan_id).unwrap_or_else(|| BillingPlan {
            id: acct.plan_id.clone(),
            name: acct.plan_id.clone(),
            ..Default::default()
        });
        BillingAccountPlan {
            scope: Some(scope.to_proto()),
            plan: Some(plan),
            cycle: acct.cycle,
            seats_purchased: acct.seats_purchased,
            seats_consumed: consumed,
            cycle_started_at: Some(ts(acct.term_start_ms)),
            next_billing_at: Some(ts(term_end_ms(acct.term_start_ms, acct.cycle))),
            pending_plan_id: acct.pending_plan_id.clone(),
            pending_seats: acct.pending_seats,
            billing_disabled: !self.cfg.enabled,
        }
    }

    /// Changes an account's plan, seats or cycle.
    ///
    /// - A higher plan, more seats or a cycle change takes effect now.
    /// - A lower plan or fewer seats is scheduled for the end of the term
    ///   (`pending_plan_id`, `pending_seats`); asking for the current plan and seats again
    ///   cancels a scheduled change.
    /// - Explicitly requesting fewer seats than are consumed is `INVALID_STATE`.
    /// - Plans that are not self-serve are `INVALID_STATE` / `contact_sales`; use
    ///   [`Billing::assign_plan`] for those.
    ///
    /// Plans are ranked by monthly seat price. Nothing is prorated.
    pub async fn change_plan(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        req: PlanChange,
    ) -> Result<BillingAccountPlan> {
        self.require_enabled()?;
        if req.plan_id.is_empty() {
            return Err(ForgeError::missing("plan_id"));
        }
        if req.seats > MAX_SEATS {
            return Err(ForgeError::too_large(
                "seats",
                u64::from(MAX_SEATS),
                u64::from(req.seats),
            ));
        }
        let plan = self
            .plan(&req.plan_id)
            .ok_or_else(|| ForgeError::not_found(format!("billing plan {}", req.plan_id)))?;
        let program_admin =
            !cx.chain.is_empty() && cx.chain.iter().all(|p| self.cfg.program_admins.contains(p));
        if !plan.self_serve && !program_admin {
            return Err(ForgeError::invalid_state(format!(
                "plan {} is not self-serve; contact sales",
                plan.id
            ))
            .with_subcode("contact_sales"));
        }
        let consumed = self.seats(cx, scope).await?.len() as u32;
        let paid = plan.seat_monthly_price_micro_usd > 0;
        if paid && req.seats > 0 && req.seats < consumed {
            return Err(ForgeError::invalid_state(format!(
                "{} seats requested but {consumed} are consumed",
                req.seats
            ))
            .with_subcode("fewer_seats_than_consumed"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut acct, row) = self.load_account(&mut tx, scope).await?;
        let before = json(&self.account_view(scope, &acct, consumed));
        let new_seats = if !paid {
            0
        } else if req.seats > 0 {
            req.seats
        } else if acct.seats_purchased > 0 {
            acct.seats_purchased
        } else {
            consumed.max(1)
        };
        if req.cycle != BillingCycle::Unspecified {
            acct.cycle = req.cycle as i32;
        }
        let (cur, new) = (self.rank(&acct.plan_id), self.rank(&plan.id));
        if plan.id == acct.plan_id {
            if new_seats < acct.seats_purchased {
                acct.pending_plan_id.clear();
                acct.pending_seats = new_seats;
            } else {
                acct.seats_purchased = new_seats;
                acct.pending_plan_id.clear();
                acct.pending_seats = 0;
            }
        } else if new > cur {
            acct.plan_id = plan.id.clone();
            acct.seats_purchased = new_seats;
            acct.pending_plan_id.clear();
            acct.pending_seats = 0;
        } else {
            acct.pending_plan_id = plan.id.clone();
            acct.pending_seats = new_seats;
        }
        acct.updated_ms = tx.now_ms();
        let view = self.account_view(scope, &acct, consumed);
        Self::save_account(&mut tx, scope, &acct, row).await?;
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.plan_changed",
                action: "billing.plan.change",
                target: &scope.key(),
                before: Some(&before),
                resource: &json(&view),
                event_org: &super::event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(view)
    }

    /// Puts an account on any plan, now, without the self-serve and proration rules: the
    /// sales and comping path, called by operators rather than the account's own admins.
    pub async fn assign_plan(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        req: PlanChange,
    ) -> Result<BillingAccountPlan> {
        self.require_enabled()?;
        let plan = self
            .plan(&req.plan_id)
            .ok_or_else(|| ForgeError::not_found(format!("billing plan {}", req.plan_id)))?;
        let consumed = self.seats(cx, scope).await?.len() as u32;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut acct, row) = self.load_account(&mut tx, scope).await?;
        let before = json(&self.account_view(scope, &acct, consumed));
        acct.plan_id = plan.id.clone();
        acct.seats_purchased = if plan.seat_monthly_price_micro_usd == 0 {
            0
        } else if req.seats > 0 {
            req.seats
        } else {
            consumed.max(1)
        };
        if req.cycle != BillingCycle::Unspecified {
            acct.cycle = req.cycle as i32;
        }
        acct.pending_plan_id.clear();
        acct.pending_seats = 0;
        acct.updated_ms = tx.now_ms();
        let view = self.account_view(scope, &acct, consumed);
        Self::save_account(&mut tx, scope, &acct, row).await?;
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.plan_changed",
                action: "billing.plan.assign",
                target: &scope.key(),
                before: Some(&before),
                resource: &json(&view),
                event_org: &super::event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(view)
    }

    /// One page of seats ordered by principal id, optionally only those attributed to a cost
    /// center; returns the page, the total consumed in the scope and the next token.
    pub async fn list_seats(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        cost_center_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<BillingSeat>, u32, String)> {
        let seats = self.seats(cx, scope).await?;
        let total = seats.len() as u32;
        let assign = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::assignments(&mut tx, &scope.key()).await?
        };
        if !cost_center_id.is_empty() {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            crate::enterprise::common::get_or_404::<bee_proto::v1::CostCenter>(
                &mut tx,
                &super::COST_CENTERS,
                &scope.key(),
                cost_center_id,
                "cost center",
            )
            .await?;
        }
        let limit = limit.clamp(1, crate::enterprise::common::MAX_LIST);
        let mut out = Vec::new();
        let mut next = String::new();
        for s in seats {
            if !after.is_empty() && s.principal.id.as_str() <= after {
                continue;
            }
            let cc = assign
                .get(&format!("principal:{}", s.principal.id))
                .or_else(|| assign.get(&format!("org:{}", s.org_id)))
                .cloned()
                .unwrap_or_default();
            if !cost_center_id.is_empty() && cc != cost_center_id {
                continue;
            }
            if out.len() == limit {
                next = out
                    .last()
                    .map(|b: &BillingSeat| b.principal.as_ref().map(|p| p.id.clone()))
                    .and_then(|x| x)
                    .unwrap_or_default();
                break;
            }
            out.push(BillingSeat {
                principal: Some(s.principal),
                org: s.org_id,
                outside_collaborator: s.outside_collaborator,
                cost_center_id: cc,
                consumed_since: (s.consumed_since_ms != 0).then(|| ts(s.consumed_since_ms)),
                last_activity_at: None,
            });
        }
        Ok((out, total, next))
    }
}
