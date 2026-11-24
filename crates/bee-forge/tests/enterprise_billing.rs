//! Billing: plans, seats, metering, pricing, budgets and limits, invoices, cost centers and
//! the `BillingService` permission matrix. Store flows run on memory and SQLite (and Postgres
//! with `BEE_TEST_PG`).

use crate::billing_models_backends as backends;
use crate::common;
use crate::enterprise_support as support;

use std::sync::Arc;

use backends::{DAY, HOUR, T0, on_backends};
use bee_forge::enterprise::billing::{
    Billing, BillingConfig, BillingService, PaymentMethodInput, PlanChange, UsageIngestor,
    UsageInput, UsageQuery, UsageReport,
};
use bee_forge::enterprise::common::{OrgScopeResolver, ResolvedScope, ScopeResolver, SpendGate};
use bee_forge::events::org_log;
use bee_forge::hooks::PrincipalDirectory;
use bee_forge::ids::ManualClock;
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::billing_budget_target::Target;
use bee_proto::v1::billing_service_server::BillingService as BillingRpc;
use bee_proto::v1::{
    AddCostCenterResourcesRequest, BillingBudgetTarget, BillingCycle, BillingPaymentMethodKind,
    BillingProduct, BillingUsageGrouping, ChangeBillingPlanRequest, CreateBillingBudgetRequest,
    CreateCostCenterRequest, DeleteBillingBudgetRequest, DeleteCostCenterRequest, EnterpriseScope,
    ErrorReason, Event, GetAccountBillingPlanRequest, GetBillingBudgetRequest,
    GetBillingEmailSettingsRequest, GetBillingPaymentMethodRequest, GetCostCenterRequest,
    GetInvoiceRequest, InvoiceStatus, ListBillingBudgetsRequest, ListBillingPlansRequest,
    ListBillingSeatsRequest, ListCostCentersRequest, ListInvoicesRequest, Org, PageRequest,
    Principal, PrincipalKind, QueryBillingUsageRequest, RemoveCostCenterResourcesRequest, Repo,
    RepoRef, RepoRole, Role, UpdateBillingBudgetRequest, UpdateBillingEmailSettingsRequest,
    UpdateCostCenterRequest, enterprise_scope, repo_ref,
};

const WS: BillingProduct = BillingProduct::WorkspacesHours;
const WS_SKU: &str = "workspaces_2core_hour";

fn owner() -> Cx {
    support::cx("owner")
}

struct Fx {
    env: common::Env,
    billing: Arc<Billing>,
    clock: Arc<ManualClock>,
    org: Org,
    scope: ResolvedScope,
}

async fn fx(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>) -> Fx {
    fx_with(store, clock, BillingConfig::default()).await
}

async fn fx_with(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>, cfg: BillingConfig) -> Fx {
    let env = common::env_with(store, clock.clone()).await;
    let name = format!("acme{}", env.forge.db.ids().ulid().unwrap().to_lowercase());
    let org = support::make_org(&env, &name).await;
    support::add_member(&env, &org, "admin", Role::Admin).await;
    support::add_member(&env, &org, "writer", Role::BillingManager).await;
    support::add_member(&env, &org, "reader", Role::Member).await;
    let billing = Billing::new(env.forge.clone(), cfg);
    let scope = OrgScopeResolver::new(env.forge.clone())
        .resolve_org(&owner(), &org.name)
        .await
        .unwrap();
    Fx {
        env,
        billing,
        clock,
        org,
        scope,
    }
}

fn input(org: &Org, product: BillingProduct, sku: &str, qty: f64, unit: &str) -> UsageInput {
    UsageInput {
        org_id: org.id.clone(),
        principal_id: "owner".into(),
        product,
        sku: sku.into(),
        quantity: qty,
        unit: unit.into(),
        ..UsageInput::default()
    }
}

fn hours(org: &Org, h: f64) -> UsageInput {
    input(org, WS, WS_SKU, h, "hour")
}

fn keyed(mut u: UsageInput, key: &str) -> UsageInput {
    u.idempotency_key = key.into();
    u
}

impl Fx {
    async fn record(&self, u: UsageInput) -> bee_forge::enterprise::billing::UsageOutcome {
        self.billing.record_usage(&owner(), u).await.unwrap()
    }

    async fn query(&self, q: UsageQuery) -> UsageReport {
        self.billing
            .query_usage(&owner(), &self.scope, q)
            .await
            .unwrap()
    }

    async fn events(&self, kind: &str) -> Vec<Event> {
        self.env.forge.flush().await.unwrap();
        self.env
            .forge
            .events
            .read(
                &owner(),
                &org_log(&self.org.id),
                0,
                &[kind.to_string()],
                1000,
            )
            .await
            .unwrap()
            .0
    }

    async fn repo(&self, name: &str) -> Repo {
        support::make_repo(&self.env, &self.org, name).await
    }

    fn hard_stop_budget(&self, amount: u64, product: BillingProduct) -> CreateBillingBudgetRequest {
        CreateBillingBudgetRequest {
            name: "cap".into(),
            product: product as i32,
            monthly_amount_micro_usd: amount,
            hard_stop: true,
            ..Default::default()
        }
    }
}

fn err(r: Result<impl std::fmt::Debug, ForgeError>) -> ForgeError {
    r.expect_err("expected an error")
}

fn repo_ref_of(r: &Repo) -> RepoRef {
    RepoRef {
        repo: Some(repo_ref::Repo::Id(r.id.clone())),
    }
}

fn scope_msg(org: &Org) -> Option<EnterpriseScope> {
    Some(EnterpriseScope {
        scope: Some(enterprise_scope::Scope::Org(org.name.clone())),
    })
}

// ---------- metering ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_metering_is_idempotent_by_key() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let a = f.record(keyed(hours(&f.org, 2.0), "job-1")).await;
        assert!(!a.duplicate);
        let b = f.record(keyed(hours(&f.org, 2.0), "job-1")).await;
        assert!(b.duplicate);
        assert_eq!(a.record.id, b.record.id);
        // The same key in another org is another record.
        let other = support::make_org(&f.env, &format!("other{}", f.org.name)).await;
        let c = f.record(keyed(hours(&other, 1.0), "job-1")).await;
        assert!(!c.duplicate);
        // Records without a key always count.
        f.record(hours(&f.org, 1.0)).await;
        f.record(hours(&f.org, 1.0)).await;
        let r = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Product],
                ..Default::default()
            })
            .await;
        assert_eq!(r.line_items.len(), 1);
        assert_eq!(r.line_items[0].quantity, 4.0);
        assert_eq!(r.line_items[0].gross_micro_usd, 4 * 90_000);
        // Bad input is refused with a reason.
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let e = err(f.billing.record_usage(&owner(), hours(&f.org, bad)).await);
            assert_eq!(e.reason, ErrorReason::InvalidArgument);
            assert_eq!(e.subcode, "invalid_quantity");
        }
        let mut no_sku = hours(&f.org, 1.0);
        no_sku.sku.clear();
        assert_eq!(
            err(f.billing.record_usage(&owner(), no_sku).await).subcode,
            "invalid_sku"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_usage_is_ingested_from_events() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let ingestor = UsageIngestor::new(f.billing.clone());
        f.env.forge.events.add_listener(ingestor.clone());
        let repo = f.repo("app").await;
        let ev = |payload: serde_json::Value| Event {
            kind: "usage.recorded".into(),
            org_id: f.org.id.clone(),
            repo_id: repo.id.clone(),
            principal: "writer".into(),
            payload_json: payload.to_string(),
            ..Default::default()
        };
        let ci = serde_json::json!({
            "product": "actions_minutes", "sku": "actions_linux_2core",
            "quantity": 30, "unit": "minute", "principal_id": "ci", "key": "run-1"
        });
        f.env
            .forge
            .events
            .append(&owner(), ev(ci.clone()), "d1")
            .await
            .unwrap();
        // Delivered twice: the key makes the second a no-op.
        f.env
            .forge
            .events
            .append(&owner(), ev(ci), "d1")
            .await
            .unwrap();
        // No key: the event's log position is the key; repo and principal default from the
        // event; the product may be spelled as the enum name.
        let storage = serde_json::json!({
            "product": "BILLING_PRODUCT_STORAGE_GB", "sku": "storage_gb_month",
            "quantity": "0.5", "unit": "GB-month"
        });
        f.env
            .forge
            .events
            .append(&owner(), ev(storage), "d2")
            .await
            .unwrap();
        // Malformed events are counted, not recorded.
        f.env
            .forge
            .events
            .append(&owner(), ev(serde_json::json!({"product": "nope"})), "d3")
            .await
            .unwrap();
        assert_eq!(ingestor.failed(), 1);
        let r = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Product, BillingUsageGrouping::Repo],
                ..Default::default()
            })
            .await;
        assert_eq!(r.line_items.len(), 2);
        assert_eq!(
            r.line_items[0].product,
            BillingProduct::ActionsMinutes as i32
        );
        assert_eq!(r.line_items[0].quantity, 30.0);
        assert_eq!(r.line_items[1].quantity, 0.5);
        for l in &r.line_items {
            assert_eq!(
                l.repo,
                Some(repo_ref_of(&repo)),
                "repo defaults to the event's"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_pricing_discounts_and_grouping() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let app = f.repo("app").await;
        // Free plan includes 2000 actions minutes a month; first come, first covered.
        let mut a = input(
            &f.org,
            BillingProduct::ActionsMinutes,
            "actions_linux_2core",
            1500.0,
            "minute",
        );
        a.repo_id = app.id.clone();
        f.record(a).await;
        f.clock.advance(HOUR);
        f.record(input(
            &f.org,
            BillingProduct::ActionsMinutes,
            "actions_linux_4core",
            1000.0,
            "minute",
        ))
        .await;
        f.clock.advance(DAY);
        f.record(hours(&f.org, 10.0)).await;

        let by_product = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Product],
                ..Default::default()
            })
            .await;
        assert_eq!(by_product.line_items.len(), 2);
        let actions = &by_product.line_items[0];
        assert_eq!(actions.product, BillingProduct::ActionsMinutes as i32);
        assert_eq!(actions.quantity, 2500.0);
        assert_eq!(actions.unit, "minute");
        assert_eq!(actions.unit_price_micro_usd, 0, "two prices in one line");
        assert_eq!(actions.gross_micro_usd, 12_000_000 + 16_000_000);
        assert_eq!(actions.discount_micro_usd, 12_000_000 + 8_000_000);
        assert_eq!(actions.net_micro_usd, 8_000_000);
        let ws = &by_product.line_items[1];
        assert_eq!(ws.gross_micro_usd, 900_000);
        assert_eq!(ws.discount_micro_usd, 0);
        assert_eq!(by_product.total_gross_micro_usd, 28_900_000);
        assert_eq!(by_product.total_net_micro_usd, 8_900_000);

        let by_sku = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Sku],
                ..Default::default()
            })
            .await;
        let skus: Vec<&str> = by_sku.line_items.iter().map(|l| l.sku.as_str()).collect();
        assert_eq!(skus, ["actions_linux_2core", "actions_linux_4core", WS_SKU]);
        assert_eq!(by_sku.line_items[1].unit_price_micro_usd, 16_000);
        assert_eq!(by_sku.total_net_micro_usd, 8_900_000);

        let by_day = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Day],
                ..Default::default()
            })
            .await;
        let days: Vec<&str> = by_day.line_items.iter().map(|l| l.date.as_str()).collect();
        assert_eq!(days, ["2026-10-18", "2026-10-19"]);
        assert_eq!(by_day.line_items[1].unit, "hour");
        let by_month = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Month],
                ..Default::default()
            })
            .await;
        assert_eq!(by_month.line_items.len(), 1);
        assert_eq!(by_month.line_items[0].date, "2026-10");
        assert_eq!(
            by_month.line_items[0].unit, "",
            "minutes and hours do not add"
        );

        // Repo, org and filters.
        let by_repo = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Repo, BillingUsageGrouping::Org],
                products: vec![BillingProduct::ActionsMinutes],
                ..Default::default()
            })
            .await;
        assert_eq!(by_repo.line_items.len(), 2);
        assert!(by_repo.line_items.iter().all(|l| l.org == f.org.id));
        let one_repo = f
            .query(UsageQuery {
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::FullName(app.full_name.clone())),
                }),
                ..Default::default()
            })
            .await;
        assert_eq!(one_repo.total_gross_micro_usd, 12_000_000);

        // The window is [start, end).
        let day2 = f
            .query(UsageQuery {
                start_ms: Some(T0 + DAY),
                end_ms: Some(T0 + 2 * DAY),
                ..Default::default()
            })
            .await;
        assert_eq!(day2.total_gross_micro_usd, 900_000);
        let none = f
            .query(UsageQuery {
                start_ms: Some(T0 + 2 * DAY),
                end_ms: Some(T0 + 3 * DAY),
                ..Default::default()
            })
            .await;
        assert!(none.line_items.is_empty());
        let e = err(f
            .billing
            .query_usage(
                &owner(),
                &f.scope,
                UsageQuery {
                    start_ms: Some(T0 + DAY),
                    end_ms: Some(T0),
                    ..Default::default()
                },
            )
            .await);
        assert_eq!(e.subcode, "invalid_window");
        assert_eq!(by_day.data_complete_until_ms, T0 + DAY + HOUR - 300_000);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_usage_paging_covers_every_line_and_totals_span_pages() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        for i in 0..5 {
            f.record(input(
                &f.org,
                BillingProduct::PackagesTransfer,
                &format!("pkg_{i}"),
                1.0,
                "GB",
            ))
            .await;
        }
        let mut token = String::new();
        let mut seen = Vec::new();
        let mut totals = Vec::new();
        loop {
            let r = f
                .query(UsageQuery {
                    group_by: vec![BillingUsageGrouping::Sku],
                    after: token.clone(),
                    limit: 2,
                    ..Default::default()
                })
                .await;
            assert!(r.line_items.len() <= 2);
            seen.extend(r.line_items.iter().map(|l| l.sku.clone()));
            totals.push(r.total_gross_micro_usd);
            if r.next_token.is_empty() {
                break;
            }
            token = r.next_token;
        }
        assert_eq!(seen, ["pkg_0", "pkg_1", "pkg_2", "pkg_3", "pkg_4"]);
        assert_eq!(totals.len(), 3);
        assert!(totals.iter().all(|t| *t == totals[0]));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_usage_query_is_deterministic() {
    // The same records, recorded in opposite orders into two forges, price identically.
    async fn run(reverse: bool) -> UsageReport {
        let f = fx(
            Arc::new(bee_forge::store::memory::MemoryStore::new()),
            backends::clock(),
        )
        .await;
        let mut plan: Vec<(u64, UsageInput)> = vec![
            (
                0,
                input(
                    &f.org,
                    BillingProduct::ActionsMinutes,
                    "actions_linux_2core",
                    1200.0,
                    "minute",
                ),
            ),
            (
                HOUR,
                input(
                    &f.org,
                    BillingProduct::ActionsMinutes,
                    "actions_linux_2core",
                    900.0,
                    "minute",
                ),
            ),
            (
                2 * HOUR,
                input(
                    &f.org,
                    BillingProduct::ActionsMinutes,
                    "actions_linux_8core",
                    300.0,
                    "minute",
                ),
            ),
            (DAY, hours(&f.org, 3.0)),
        ];
        if reverse {
            plan.reverse();
        }
        for (offset, mut u) in plan {
            u.at_ms = T0 + offset;
            u.idempotency_key = format!("k{offset}");
            f.record(u).await;
        }
        f.clock.set(T0 + 2 * DAY);
        f.query(UsageQuery {
            group_by: vec![BillingUsageGrouping::Day, BillingUsageGrouping::Sku],
            ..Default::default()
        })
        .await
    }
    let (a, b) = (run(false).await, run(true).await);
    assert_eq!(a, b);
    assert_eq!(run(false).await, a, "and the same twice");
    // 2000 included minutes cover 1200 + 800 of the 900; the 8-core minutes are all billed.
    assert_eq!(a.total_net_micro_usd, 100 * 8_000 + 300 * 32_000 + 270_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_agents_are_attributed_through_their_owner() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        f.env.directory.register(
            Principal {
                id: "agent".into(),
                kind: PrincipalKind::Agent as i32,
                display: "agent".into(),
                owner_id: "writer".into(),
                runtime: "claude-code".into(),
                ..Default::default()
            },
            vec![],
            vec![],
        );
        let mut by_agent = hours(&f.org, 1.0);
        by_agent.principal_id = "agent".into();
        let out = f.record(by_agent).await;
        assert_eq!(out.record.principal_id, "agent");
        assert_eq!(out.record.attributed_to, "writer");
        f.record(hours(&f.org, 2.0)).await;
        let r = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::Principal],
                ..Default::default()
            })
            .await;
        let who: Vec<(&str, f64)> = r
            .line_items
            .iter()
            .map(|l| (l.principal_id.as_str(), l.quantity))
            .collect();
        assert_eq!(who, [("owner", 2.0), ("writer", 1.0)]);
    })
    .await;
}

// ---------- budgets, limits, enforcement ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_budget_alerts_fire_once_per_threshold_per_month() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        f.billing
            .update_email_settings(
                &owner(),
                &f.scope,
                UpdateBillingEmailSettingsRequest {
                    primary_email: Some("billing@example.com".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let b = f
            .billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "workspaces".into(),
                    monthly_amount_micro_usd: 1_000_000,
                    alert_threshold_percents: vec![100, 50, 50],
                    alert_recipient_ids: vec!["admin".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            b.alert_threshold_percents,
            [50, 100],
            "sorted and deduplicated"
        );

        let o = f.record(hours(&f.org, 3.0)).await; // 270k
        assert!(o.budgets[0].fired_thresholds.is_empty());
        let o = f.record(hours(&f.org, 3.0)).await; // 540k
        assert_eq!(o.budgets[0].fired_thresholds, [50]);
        let o = f.record(hours(&f.org, 1.0)).await; // 630k
        assert!(o.budgets[0].fired_thresholds.is_empty());
        let o = f.record(hours(&f.org, 6.0)).await; // 1.17M
        assert_eq!(o.budgets[0].fired_thresholds, [100]);
        assert_eq!(o.budgets[0].spent_micro_usd, 1_170_000);
        let o = f.record(hours(&f.org, 6.0)).await;
        assert!(o.budgets[0].fired_thresholds.is_empty(), "once per month");
        assert!(!o.limit_hit, "no hard stop");

        let alerts = f.events("billing.budget_threshold_reached").await;
        assert_eq!(alerts.len(), 2);
        let p: serde_json::Value = serde_json::from_str(&alerts[0].payload_json).unwrap();
        assert_eq!(p["budget_id"], b.id);
        assert_eq!(p["threshold_percent"], 50);
        assert_eq!(p["month"], "2026-10");
        assert_eq!(p["recipients"], serde_json::json!(["admin"]));
        assert_eq!(p["emails"], serde_json::json!(["billing@example.com"]));

        // The next month the thresholds fire again.
        f.clock.set(T0 + 15 * DAY);
        let o = f.record(hours(&f.org, 12.0)).await; // 1.08M in November
        assert_eq!(o.budgets[0].fired_thresholds, [50, 100]);
        assert_eq!(f.events("billing.budget_threshold_reached").await.len(), 4);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_budgets_crud_and_spent() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let repo = f.repo("app").await;
        let org_budget = f
            .billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "org".into(),
                    target: Some(BillingBudgetTarget {
                        target: Some(Target::Org(f.org.name.clone())),
                    }),
                    monthly_amount_micro_usd: 5_000_000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Names resolve to ids.
        assert_eq!(
            org_budget.target.unwrap().target,
            Some(Target::Org(f.org.id.clone()))
        );
        let repo_budget = f
            .billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "repo".into(),
                    product: WS as i32,
                    target: Some(BillingBudgetTarget {
                        target: Some(Target::Repo(RepoRef {
                            repo: Some(repo_ref::Repo::FullName(repo.full_name.clone())),
                        })),
                    }),
                    monthly_amount_micro_usd: 1_000_000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut in_repo = hours(&f.org, 2.0);
        in_repo.repo_id = repo.id.clone();
        f.record(in_repo).await;
        f.record(hours(&f.org, 1.0)).await;

        let got = f
            .billing
            .get_budget(&owner(), &f.scope, &repo_budget.id)
            .await
            .unwrap();
        assert_eq!(got.spent_micro_usd, 180_000, "only the repo's usage");
        let got = f
            .billing
            .get_budget(&owner(), &f.scope, &org_budget.id)
            .await
            .unwrap();
        assert_eq!(got.spent_micro_usd, 270_000);

        let updated = f
            .billing
            .update_budget(
                &owner(),
                &f.scope,
                UpdateBillingBudgetRequest {
                    budget_id: repo_budget.id.clone(),
                    name: Some("repo cap".into()),
                    monthly_amount_micro_usd: Some(2_000_000),
                    hard_stop: Some(true),
                    alert_threshold_percents: vec![90],
                    alert_recipient_ids: vec!["admin".into(), "admin".into()],
                    replace_alerts: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "repo cap");
        assert_eq!(updated.monthly_amount_micro_usd, 2_000_000);
        assert!(updated.hard_stop);
        assert_eq!(updated.alert_threshold_percents, [90]);
        assert_eq!(updated.alert_recipient_ids, ["admin"]);

        let (all, _) = f
            .billing
            .list_budgets(&owner(), &f.scope, BillingProduct::Unspecified, "", 50)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        let (ws, _) = f
            .billing
            .list_budgets(&owner(), &f.scope, WS, "", 50)
            .await
            .unwrap();
        assert_eq!(ws.len(), 1);
        let (page, next) = f
            .billing
            .list_budgets(&owner(), &f.scope, BillingProduct::Unspecified, "", 1)
            .await
            .unwrap();
        assert_eq!((page.len(), next.is_empty()), (1, false));
        let (rest, next) = f
            .billing
            .list_budgets(&owner(), &f.scope, BillingProduct::Unspecified, &next, 1)
            .await
            .unwrap();
        assert_eq!((rest.len(), next.is_empty()), (1, true));
        assert_ne!(page[0].id, rest[0].id);

        f.billing
            .delete_budget(&owner(), &f.scope, &repo_budget.id)
            .await
            .unwrap();
        assert_eq!(
            err(f
                .billing
                .get_budget(&owner(), &f.scope, &repo_budget.id)
                .await)
            .reason,
            ErrorReason::NotFound
        );
        // Validation.
        let bad = |req: CreateBillingBudgetRequest| async {
            err(f.billing.create_budget(&owner(), &f.scope, req).await)
        };
        assert_eq!(
            bad(CreateBillingBudgetRequest::default()).await.subcode,
            "invalid_name"
        );
        assert_eq!(
            bad(CreateBillingBudgetRequest {
                name: "x".into(),
                alert_threshold_percents: vec![0],
                ..Default::default()
            })
            .await
            .subcode,
            "invalid_threshold"
        );
        assert_eq!(
            bad(CreateBillingBudgetRequest {
                name: "x".into(),
                target: Some(BillingBudgetTarget {
                    target: Some(Target::CostCenterId("missing".into())),
                }),
                ..Default::default()
            })
            .await
            .reason,
            ErrorReason::NotFound
        );
        let other = support::make_org(&f.env, &format!("elsewhere{}", f.org.name)).await;
        assert_eq!(
            bad(CreateBillingBudgetRequest {
                name: "x".into(),
                target: Some(BillingBudgetTarget {
                    target: Some(Target::Org(other.name.clone())),
                }),
                ..Default::default()
            })
            .await
            .reason,
            ErrorReason::NotFound,
            "an org outside the account"
        );
        assert!(!f.events("billing.budget_deleted").await.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_limit_hard_stop_refuses_through_spend_gate_and_enforce() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let soft = f
            .billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "soft".into(),
                    monthly_amount_micro_usd: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let cap = f
            .billing
            .create_budget(&owner(), &f.scope, f.hard_stop_budget(500_000, WS))
            .await
            .unwrap();
        let gate: Arc<dyn SpendGate> = f.billing.clone();

        f.record(hours(&f.org, 4.0)).await; // 360k of 500k
        // 1 more hour fits (450k); 2 more would pass the amount (540k).
        f.billing
            .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 1.0)
            .await
            .unwrap();
        let e = err(f
            .billing
            .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 2.0)
            .await);
        assert_eq!(e.reason, ErrorReason::QuotaExceeded);
        assert_eq!(e.subcode, "budget_exhausted");
        assert!(e.message.contains(&cap.id));
        gate.check_spend(&owner(), &f.org.id, "", WS, 140_000)
            .await
            .unwrap();
        let e = err(gate.check_spend(&owner(), &f.org.id, "", WS, 140_001).await);
        assert_eq!(e.reason, ErrorReason::QuotaExceeded);
        // Other products are not covered; the soft budget never blocks.
        f.billing
            .enforce(
                &owner(),
                &f.org.id,
                "",
                BillingProduct::ActionsMinutes,
                "actions_linux_2core",
                1e6,
            )
            .await
            .unwrap();

        // Recording is never refused; it reports that the limit is now hit.
        let o = f.record(hours(&f.org, 2.0)).await; // 540k >= 500k
        assert!(o.limit_hit);
        let state = o.budgets.iter().find(|b| b.budget_id == cap.id).unwrap();
        assert!(state.hard_stop);
        assert_eq!(state.spent_micro_usd, 540_000);
        assert!(
            !o.budgets
                .iter()
                .find(|b| b.budget_id == soft.id)
                .unwrap()
                .hard_stop
        );
        // Spent: refused even for free work.
        assert_eq!(
            err(gate.check_spend(&owner(), &f.org.id, "", WS, 0).await).reason,
            ErrorReason::QuotaExceeded
        );
        // Raising the amount lifts the stop.
        f.billing
            .update_budget(
                &owner(),
                &f.scope,
                UpdateBillingBudgetRequest {
                    budget_id: cap.id.clone(),
                    monthly_amount_micro_usd: Some(900_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        gate.check_spend(&owner(), &f.org.id, "", WS, 0)
            .await
            .unwrap();
        // Unknown orgs are not found, not silently allowed.
        assert_eq!(
            err(gate.check_spend(&owner(), "no-such-org", "", WS, 0).await).reason,
            ErrorReason::NotFound
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_limit_default_spending_limit_applies_without_a_budget() {
    on_backends(|store, clock| async move {
        let cfg = BillingConfig {
            default_spending_limit_micro_usd: 1_000_000,
            ..BillingConfig::default()
        };
        let f = fx_with(store, clock, cfg).await;
        let o = f.record(hours(&f.org, 10.0)).await; // 900k
        let l = o.spending_limit.expect("limit state without a budget");
        assert_eq!(
            (l.limit_micro_usd, l.spent_micro_usd, l.exceeded),
            (1_000_000, 900_000, false)
        );
        assert!(!o.limit_hit);
        f.billing
            .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 1.0)
            .await
            .unwrap(); // 990k
        let e = err(f
            .billing
            .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 2.0)
            .await);
        assert_eq!(e.reason, ErrorReason::QuotaExceeded);
        assert_eq!(e.subcode, "spending_limit");
        // It covers every product, not only the one that spent.
        let e = err(f
            .billing
            .enforce(
                &owner(),
                &f.org.id,
                "",
                BillingProduct::PackagesTransfer,
                "packages_transfer_gb",
                1000.0,
            )
            .await);
        assert_eq!(e.subcode, "spending_limit");
        let o = f.record(hours(&f.org, 2.0)).await;
        assert!(o.limit_hit);
        // A budget covering the charge takes over from the default limit.
        f.billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "bigger".into(),
                    monthly_amount_micro_usd: 50_000_000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.billing
            .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 100.0)
            .await
            .unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_limit_month_rollover_resets_spend() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let cap = f
            .billing
            .create_budget(&owner(), &f.scope, f.hard_stop_budget(500_000, WS))
            .await
            .unwrap();
        f.record(hours(&f.org, 6.0)).await; // 540k in October
        assert!(
            err(f
                .billing
                .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 1.0)
                .await)
            .reason
                == ErrorReason::QuotaExceeded
        );

        // 2026-10-31T23:59 is still October.
        f.clock.set(T0 + 14 * DAY - 60_000);
        assert!(
            f.billing
                .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 1.0)
                .await
                .is_err()
        );
        // 2026-11-01T00:00 starts a fresh month.
        f.clock.set(T0 + 14 * DAY);
        f.billing
            .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 1.0)
            .await
            .unwrap();
        assert_eq!(
            f.billing
                .get_budget(&owner(), &f.scope, &cap.id)
                .await
                .unwrap()
                .spent_micro_usd,
            0
        );
        let o = f.record(hours(&f.org, 3.0)).await;
        assert_eq!(o.budgets[0].spent_micro_usd, 270_000);
        assert!(
            f.billing
                .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 3.0)
                .await
                .is_err()
        );
        // Usage backdated into October counts for October only.
        let mut late = hours(&f.org, 1.0);
        late.at_ms = T0 + DAY;
        let o = f.record(late).await;
        assert_eq!(o.budgets[0].spent_micro_usd, 630_000);
        assert_eq!(
            f.billing
                .get_budget(&owner(), &f.scope, &cap.id)
                .await
                .unwrap()
                .spent_micro_usd,
            270_000
        );
        // October stays queryable.
        let oct = f
            .query(UsageQuery {
                start_ms: Some(T0),
                end_ms: Some(T0 + 14 * DAY),
                ..Default::default()
            })
            .await;
        assert_eq!(oct.total_gross_micro_usd, 7 * 90_000);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_limit_repo_and_cost_center_budgets_cover_only_their_usage() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let (r1, r2) = (f.repo("one").await, f.repo("two").await);
        let cc = f
            .billing
            .create_cost_center(&owner(), &f.scope, "platform")
            .await
            .unwrap();
        f.billing
            .add_cost_center_resources(&owner(), &f.scope, &cc.id, &[repo_ref_of(&r1)], &[], &[])
            .await
            .unwrap();
        f.billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "platform cap".into(),
                    product: WS as i32,
                    target: Some(BillingBudgetTarget {
                        target: Some(Target::CostCenterId(cc.id.clone())),
                    }),
                    monthly_amount_micro_usd: 100_000,
                    hard_stop: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "repo two cap".into(),
                    product: WS as i32,
                    target: Some(BillingBudgetTarget {
                        target: Some(Target::Repo(repo_ref_of(&r2))),
                    }),
                    monthly_amount_micro_usd: 100_000,
                    hard_stop: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut u = hours(&f.org, 2.0);
        u.repo_id = r1.id.clone();
        let o = f.record(u).await;
        assert!(o.limit_hit);
        assert_eq!(
            o.budgets.len(),
            1,
            "only the cost center's budget covers repo one"
        );
        let e = err(f
            .billing
            .enforce(&owner(), &f.org.id, &r1.id, WS, WS_SKU, 0.1)
            .await);
        assert_eq!(e.reason, ErrorReason::QuotaExceeded);
        // Repo two has its own budget with nothing spent.
        f.billing
            .enforce(&owner(), &f.org.id, &r2.id, WS, WS_SKU, 1.0)
            .await
            .unwrap();
        assert!(
            f.billing
                .enforce(&owner(), &f.org.id, &r2.id, WS, WS_SKU, 2.0)
                .await
                .is_err()
        );
        // Work outside both is not covered.
        f.billing
            .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 100.0)
            .await
            .unwrap();
    })
    .await;
}

// ---------- seats and plans ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_seats_count_members_and_outside_collaborators() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let repo = f.repo("app").await;
        let outsider = f.env.directory.get("outsider").await.unwrap();
        let (_, inv) = f
            .env
            .forge
            .orgs
            .add_collaborator(&owner(), &repo.id, outsider.clone(), RepoRole::Read, "")
            .await
            .unwrap();
        f.env
            .forge
            .orgs
            .answer_repo_invitation(&support::cx("outsider"), &inv.unwrap().id, &outsider, true)
            .await
            .unwrap();
        // owner, admin, reader and the outside collaborator; the billing manager is free.
        let seats = f.billing.seats(&owner(), &f.scope).await.unwrap();
        let ids: Vec<&str> = seats.iter().map(|s| s.principal.id.as_str()).collect();
        assert_eq!(ids, ["admin", "outsider", "owner", "reader"]);
        assert!(seats[1].outside_collaborator && !seats[0].outside_collaborator);
        let plan = f.billing.account_plan(&owner(), &f.scope).await.unwrap();
        assert_eq!(plan.seats_consumed, 4);
        assert_eq!(plan.plan.unwrap().id, "free");

        let (page, total, next) = f
            .billing
            .list_seats(&owner(), &f.scope, "", "", 2)
            .await
            .unwrap();
        assert_eq!(total, 4);
        assert_eq!(page.len(), 2);
        assert_eq!(next, "outsider");
        let (page2, _, next2) = f
            .billing
            .list_seats(&owner(), &f.scope, "", &next, 2)
            .await
            .unwrap();
        assert_eq!(
            page2
                .iter()
                .map(|s| s.principal.as_ref().unwrap().id.as_str())
                .collect::<Vec<_>>(),
            ["owner", "reader"]
        );
        assert!(next2.is_empty());
        assert!(page[1].outside_collaborator);
        assert_eq!(page[0].org, f.org.id);

        // Seats of a cost center.
        let cc = f
            .billing
            .create_cost_center(&owner(), &f.scope, "eng")
            .await
            .unwrap();
        f.billing
            .add_cost_center_resources(&owner(), &f.scope, &cc.id, &[], &[], &["reader".into()])
            .await
            .unwrap();
        let (only, total, _) = f
            .billing
            .list_seats(&owner(), &f.scope, &cc.id, "", 50)
            .await
            .unwrap();
        assert_eq!((only.len(), total), (1, 4));
        assert_eq!(only[0].cost_center_id, cc.id);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_plan_change_rules() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let change = |plan: &str, seats: u32| PlanChange {
            plan_id: plan.into(),
            seats,
            cycle: BillingCycle::Unspecified,
        };
        let base = f.billing.account_plan(&owner(), &f.scope).await.unwrap();
        assert_eq!((base.seats_purchased, base.seats_consumed), (0, 3));
        assert_eq!(base.cycle, BillingCycle::Monthly as i32);
        assert!(!base.billing_disabled);

        // Fewer seats than consumed.
        let e = err(f
            .billing
            .change_plan(&owner(), &f.scope, change("team", 2))
            .await);
        assert_eq!(e.reason, ErrorReason::InvalidState);
        assert_eq!(e.subcode, "fewer_seats_than_consumed");
        // Upgrades take effect now; 0 seats buys what is consumed.
        let p = f
            .billing
            .change_plan(&owner(), &f.scope, change("team", 0))
            .await
            .unwrap();
        assert_eq!(
            (p.plan.as_ref().unwrap().id.as_str(), p.seats_purchased),
            ("team", 3)
        );
        let p = f
            .billing
            .change_plan(
                &owner(),
                &f.scope,
                PlanChange {
                    plan_id: "team".into(),
                    seats: 5,
                    cycle: BillingCycle::Yearly,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (p.seats_purchased, p.cycle),
            (5, BillingCycle::Yearly as i32)
        );
        // Fewer seats are scheduled for the end of the cycle.
        let p = f
            .billing
            .change_plan(&owner(), &f.scope, change("team", 4))
            .await
            .unwrap();
        assert_eq!(
            (
                p.seats_purchased,
                p.pending_seats,
                p.pending_plan_id.as_str()
            ),
            (5, 4, "")
        );
        // More seats are immediate and cancel the schedule.
        let p = f
            .billing
            .change_plan(&owner(), &f.scope, change("team", 8))
            .await
            .unwrap();
        assert_eq!((p.seats_purchased, p.pending_seats), (8, 0));
        // A lower plan is scheduled.
        let p = f
            .billing
            .change_plan(&owner(), &f.scope, change("free", 0))
            .await
            .unwrap();
        assert_eq!(p.plan.as_ref().unwrap().id, "team");
        assert_eq!((p.pending_plan_id.as_str(), p.pending_seats), ("free", 0));
        // Asking for the current plan again cancels it.
        let p = f
            .billing
            .change_plan(&owner(), &f.scope, change("team", 0))
            .await
            .unwrap();
        assert_eq!((p.pending_plan_id.as_str(), p.seats_purchased), ("", 8));
        // Plans that are not self-serve, and plans that do not exist.
        let e = err(f
            .billing
            .change_plan(&owner(), &f.scope, change("enterprise", 5))
            .await);
        assert_eq!(
            (e.reason, e.subcode.as_str()),
            (ErrorReason::InvalidState, "contact_sales")
        );
        assert_eq!(
            err(f
                .billing
                .change_plan(&owner(), &f.scope, change("platinum", 5))
                .await)
            .reason,
            ErrorReason::NotFound
        );
        assert_eq!(
            err(f
                .billing
                .change_plan(&owner(), &f.scope, change("", 5))
                .await)
            .subcode,
            "missing_field"
        );
        // Sales can assign it.
        let p = f
            .billing
            .assign_plan(&owner(), &f.scope, change("enterprise", 10))
            .await
            .unwrap();
        assert_eq!(
            (p.plan.unwrap().id.as_str(), p.seats_purchased),
            ("enterprise", 10)
        );
        assert_eq!(f.events("billing.plan_changed").await.len(), 7);
        // Plans of an org account include free; the catalog is the built-in one.
        let ids: Vec<String> = f
            .billing
            .list_plans(&f.scope)
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(
            ids,
            ["free", "team", "enterprise", "education", "nonprofit"]
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_config_overrides_plans_and_prices() {
    on_backends(|store, clock| async move {
        let mut cfg = BillingConfig {
            plans: vec![bee_proto::v1::BillingPlan {
                id: "oss".into(),
                name: "OSS".into(),
                self_serve: true,
                included: vec![bee_proto::v1::BillingIncludedUsage {
                    product: WS as i32,
                    quantity: 5.0,
                    unit: "hour".into(),
                }],
                ..Default::default()
            }],
            ..BillingConfig::default()
        };
        cfg.prices.insert(WS_SKU.into(), 1_000_000);
        cfg.prices.insert("custom_sku".into(), 7);
        let f = fx_with(store, clock, cfg).await;
        assert_eq!(f.billing.unit_price(WS_SKU), 1_000_000);
        assert_eq!(f.billing.unit_price("custom_sku"), 7);
        assert_eq!(
            f.billing.unit_price("agent_tokens_input"),
            3,
            "built-in prices remain"
        );
        assert_eq!(f.billing.unit_price("unknown"), 0);
        let p = f.billing.account_plan(&owner(), &f.scope).await.unwrap();
        assert_eq!(
            p.plan.unwrap().id,
            "oss",
            "the first free plan is the default"
        );
        f.record(hours(&f.org, 8.0)).await;
        let r = f.query(UsageQuery::default()).await;
        // 5 hours included at 1 USD each.
        assert_eq!(r.total_gross_micro_usd, 8_000_000);
        assert_eq!(r.total_net_micro_usd, 3_000_000);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_disabled_still_meters_and_refuses_what_needs_a_bill() {
    on_backends(|store, clock| async move {
        let cfg = BillingConfig {
            enabled: false,
            ..BillingConfig::default()
        };
        let f = fx_with(store, clock, cfg).await;
        f.record(hours(&f.org, 1.0)).await;
        assert_eq!(
            f.query(UsageQuery::default()).await.total_gross_micro_usd,
            90_000
        );
        assert!(
            f.billing
                .account_plan(&owner(), &f.scope)
                .await
                .unwrap()
                .billing_disabled
        );
        let gone = |e: ForgeError| (e.reason, e.subcode);
        let want = (ErrorReason::Unimplemented, "billing_disabled".to_string());
        assert_eq!(
            gone(err(f
                .billing
                .change_plan(
                    &owner(),
                    &f.scope,
                    PlanChange {
                        plan_id: "team".into(),
                        ..Default::default()
                    }
                )
                .await)),
            want
        );
        assert_eq!(
            gone(err(f
                .billing
                .close_cycle(&owner(), &f.scope, T0 + 40 * DAY)
                .await)),
            want
        );
        assert_eq!(
            gone(err(f
                .billing
                .list_invoices(&owner(), &f.scope, InvoiceStatus::Unspecified, "", 10)
                .await)),
            want
        );
        assert_eq!(
            gone(err(f.billing.get_payment_method(&owner(), &f.scope).await)),
            want
        );
        assert_eq!(
            gone(err(f.billing.get_email_settings(&owner(), &f.scope).await)),
            want
        );
        // Budgets and cost centers keep working for quotas and attribution.
        f.billing
            .create_budget(&owner(), &f.scope, f.hard_stop_budget(50_000, WS))
            .await
            .unwrap();
        assert!(
            f.billing
                .enforce(&owner(), &f.org.id, "", WS, WS_SKU, 1.0)
                .await
                .is_err()
        );
        f.billing
            .create_cost_center(&owner(), &f.scope, "eng")
            .await
            .unwrap();
    })
    .await;
}

// ---------- invoices ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_invoices_are_generated_deterministically_and_follow_a_lifecycle() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        f.billing
            .change_plan(
                &owner(),
                &f.scope,
                PlanChange {
                    plan_id: "team".into(),
                    seats: 3,
                    cycle: BillingCycle::Monthly,
                },
            )
            .await
            .unwrap();
        // Team includes 3000 actions minutes: 500 are billed. Workspaces have no allowance.
        f.record(input(
            &f.org,
            BillingProduct::ActionsMinutes,
            "actions_linux_2core",
            3500.0,
            "minute",
        ))
        .await;
        f.record(hours(&f.org, 10.0)).await;

        // Nothing is due while October runs.
        assert!(
            f.billing
                .close_cycle(&owner(), &f.scope, T0 + 5 * DAY)
                .await
                .unwrap()
                .is_empty()
        );

        let now = T0 + 20 * DAY; // 2026-11-07
        f.clock.set(now);
        let made = f
            .billing
            .close_cycle(&owner(), &f.scope, now)
            .await
            .unwrap();
        assert_eq!(made.len(), 1);
        let inv = &made[0];
        assert!(inv.number.starts_with("INV-202610-"), "{}", inv.number);
        assert_eq!(inv.status, InvoiceStatus::Open as i32);
        assert_eq!(inv.subtotal_micro_usd, 12_000_000 + 4_000_000 + 900_000);
        assert_eq!((inv.tax_micro_usd, inv.total_micro_usd), (0, 16_900_000));
        assert_eq!(
            inv.period_start_at.as_ref().unwrap().seconds,
            (T0 - 17 * DAY) as i64 / 1000
        );
        assert_eq!(
            inv.period_end_at.as_ref().unwrap().seconds,
            (T0 + 14 * DAY) as i64 / 1000
        );
        assert_eq!(inv.line_items.len(), 3);
        assert_eq!(inv.line_items[0].product, BillingProduct::Seats as i32);
        assert_eq!(
            (
                inv.line_items[0].quantity,
                inv.line_items[0].amount_micro_usd
            ),
            (3.0, 12_000_000)
        );
        assert_eq!(inv.line_items[1].sku, "actions_linux_2core");
        assert_eq!(
            (
                inv.line_items[1].quantity,
                inv.line_items[1].amount_micro_usd
            ),
            (500.0, 4_000_000)
        );
        assert_eq!(inv.line_items[2].sku, WS_SKU);

        // A term is invoiced once.
        assert!(
            f.billing
                .close_cycle(&owner(), &f.scope, now)
                .await
                .unwrap()
                .is_empty()
        );
        // Two more months at once: seats only.
        let later = T0 + 80 * DAY; // 2027-01-05
        f.clock.set(later);
        let more = f
            .billing
            .close_cycle(&owner(), &f.scope, later)
            .await
            .unwrap();
        assert_eq!(more.len(), 2);
        assert!(
            more[0].number.starts_with("INV-202611-") && more[1].number.starts_with("INV-202612-")
        );
        assert!(more.iter().all(|i| i.total_micro_usd == 12_000_000));

        // Newest first, with a status filter and paging; the list leaves out line items.
        let (all, next) = f
            .billing
            .list_invoices(&owner(), &f.scope, InvoiceStatus::Unspecified, "", 50)
            .await
            .unwrap();
        let numbers: Vec<&str> = all.iter().map(|i| i.number.as_str()).collect();
        assert!(
            numbers[0].starts_with("INV-202612-") && numbers[2].starts_with("INV-202610-"),
            "{numbers:?}"
        );
        assert!(next.is_empty() && all.iter().all(|i| i.line_items.is_empty()));
        let (p1, next) = f
            .billing
            .list_invoices(&owner(), &f.scope, InvoiceStatus::Open, "", 2)
            .await
            .unwrap();
        assert_eq!(p1.len(), 2);
        let (p2, next2) = f
            .billing
            .list_invoices(&owner(), &f.scope, InvoiceStatus::Open, &next, 2)
            .await
            .unwrap();
        assert_eq!((p2.len(), next2.is_empty()), (1, true));
        assert!(p2[0].number.starts_with("INV-202610-"));
        assert!(
            f.billing
                .list_invoices(&owner(), &f.scope, InvoiceStatus::Paid, "", 5)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        // Signed PDF URL.
        let (got, url) = f
            .billing
            .get_invoice(&owner(), &f.scope, &inv.id)
            .await
            .unwrap();
        assert_eq!(got.line_items.len(), 3);
        assert!(url.url.starts_with(&format!(
            "https://beehive.example/billing/invoices/{}.pdf?exp=",
            inv.id
        )));
        assert_eq!(url.expires_at_ms, later + 300_000);
        assert_eq!(
            f.billing.verify_invoice_url(&url.url, later).unwrap(),
            inv.id
        );
        assert_eq!(
            err(f.billing.verify_invoice_url(&url.url, later + 300_000)).subcode,
            "url_expired"
        );
        let forged = url.url.replace(&inv.id, &more[0].id);
        assert_eq!(
            err(f.billing.verify_invoice_url(&forged, later)).subcode,
            "bad_signature"
        );
        let mut tampered = url.url.clone();
        tampered.pop();
        tampered.push(if url.url.ends_with('0') { '1' } else { '0' });
        assert_eq!(
            err(f.billing.verify_invoice_url(&tampered, later)).subcode,
            "bad_signature"
        );
        assert_eq!(
            err(f.billing.verify_invoice_url("https://x/y", later)).subcode,
            "bad_invoice_url"
        );
        assert_eq!(
            err(f.billing.get_invoice(&owner(), &f.scope, "nope").await).reason,
            ErrorReason::NotFound
        );

        // Status lifecycle.
        let paid = f
            .billing
            .mark_invoice_paid(&owner(), &f.scope, &inv.id, later)
            .await
            .unwrap();
        assert_eq!(paid.status, InvoiceStatus::Paid as i32);
        assert_eq!(paid.amount_paid_micro_usd, paid.total_micro_usd);
        assert!(paid.paid_at.is_some());
        let e = err(f
            .billing
            .void_invoice(&owner(), &f.scope, &inv.id, later)
            .await);
        assert_eq!(e.reason, ErrorReason::InvalidState);
        assert_eq!(
            f.billing
                .mark_invoice_paid(&owner(), &f.scope, &inv.id, later)
                .await
                .unwrap()
                .status,
            InvoiceStatus::Paid as i32,
            "paying twice is a no-op"
        );
        let voided = f
            .billing
            .void_invoice(&owner(), &f.scope, &more[0].id, later)
            .await
            .unwrap();
        assert_eq!(voided.status, InvoiceStatus::Void as i32);
        assert_eq!(
            err(f
                .billing
                .mark_invoice_paid(&owner(), &f.scope, &more[0].id, later)
                .await)
            .reason,
            ErrorReason::InvalidState
        );
        let bad = f
            .billing
            .mark_invoice_uncollectible(&owner(), &f.scope, &more[1].id, later)
            .await
            .unwrap();
        assert_eq!(bad.status, InvoiceStatus::Uncollectible as i32);
        assert_eq!(
            f.billing
                .mark_invoice_paid(&owner(), &f.scope, &more[1].id, later)
                .await
                .unwrap()
                .status,
            InvoiceStatus::Paid as i32,
            "late payment"
        );
        assert_eq!(
            f.billing
                .list_invoices(&owner(), &f.scope, InvoiceStatus::Paid, "", 5)
                .await
                .unwrap()
                .0
                .len(),
            2
        );

        // Drafts stay drafts until finalized.
        let feb = T0 + 110 * DAY; // 2027-02-04
        f.clock.set(feb);
        let drafts = f
            .billing
            .close_cycle_draft(&owner(), &f.scope, feb)
            .await
            .unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].status, InvoiceStatus::Draft as i32);
        assert!(drafts[0].issued_at.is_none());
        assert!(
            err(f
                .billing
                .mark_invoice_paid(&owner(), &f.scope, &drafts[0].id, feb)
                .await)
            .reason
                == ErrorReason::InvalidState
        );
        let open = f
            .billing
            .finalize_invoice(&owner(), &f.scope, &drafts[0].id, feb)
            .await
            .unwrap();
        assert_eq!(open.status, InvoiceStatus::Open as i32);
        assert!(open.issued_at.is_some() && open.due_at.is_some());
        assert_eq!(f.events("billing.invoice_created").await.len(), 4);
        assert!(!f.events("billing.invoice_updated").await.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_invoice_applies_pending_changes_tax_and_skips_empty_terms() {
    on_backends(|store, clock| async move {
        let cfg = BillingConfig {
            tax_rate_bps: 825,
            ..BillingConfig::default()
        };
        let f = fx_with(store, clock, cfg).await;
        // The free plan with no billable usage produces no invoice but still advances.
        f.record(hours(&f.org, 0.0001)).await; // 9 micro-USD
        let now = T0 + 20 * DAY;
        f.clock.set(now);
        let none = f
            .billing
            .close_cycle(&owner(), &f.scope, now)
            .await
            .unwrap();
        assert_eq!(none.len(), 1, "9 micro-USD is a real amount");
        assert_eq!((none[0].subtotal_micro_usd, none[0].tax_micro_usd), (9, 1));

        // Team with a scheduled downgrade: the term is billed at team, then free applies.
        f.billing
            .change_plan(
                &owner(),
                &f.scope,
                PlanChange {
                    plan_id: "team".into(),
                    seats: 4,
                    cycle: BillingCycle::Monthly,
                },
            )
            .await
            .unwrap();
        f.billing
            .change_plan(
                &owner(),
                &f.scope,
                PlanChange {
                    plan_id: "free".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let dec = T0 + 50 * DAY; // 2026-12-07
        f.clock.set(dec);
        let invoices = f
            .billing
            .close_cycle(&owner(), &f.scope, dec)
            .await
            .unwrap();
        assert_eq!(
            invoices.len(),
            1,
            "November, billed at team; December is still open"
        );
        assert_eq!(invoices[0].subtotal_micro_usd, 16_000_000);
        assert_eq!(invoices[0].tax_micro_usd, 1_320_000);
        let after = f.billing.account_plan(&owner(), &f.scope).await.unwrap();
        assert_eq!(after.plan.unwrap().id, "free");
        assert_eq!(
            (after.seats_purchased, after.pending_plan_id.as_str()),
            (0, "")
        );
        let jan = T0 + 80 * DAY;
        f.clock.set(jan);
        assert!(
            f.billing
                .close_cycle(&owner(), &f.scope, jan)
                .await
                .unwrap()
                .is_empty(),
            "free and idle"
        );
    })
    .await;
}

// ---------- cost centers ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_cost_centers_attribute_usage_and_move_resources() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let (r1, r2) = (f.repo("one").await, f.repo("two").await);
        f.env.directory.register(
            Principal {
                id: "agent".into(),
                kind: PrincipalKind::Agent as i32,
                display: "agent".into(),
                owner_id: "writer".into(),
                ..Default::default()
            },
            vec![],
            vec![],
        );
        let a = f
            .billing
            .create_cost_center(&owner(), &f.scope, "Platform")
            .await
            .unwrap();
        let b = f
            .billing
            .create_cost_center(&owner(), &f.scope, "Product")
            .await
            .unwrap();
        assert_eq!(
            err(f
                .billing
                .create_cost_center(&owner(), &f.scope, "platform")
                .await)
            .reason,
            ErrorReason::AlreadyExists
        );
        assert_eq!(
            err(f.billing.create_cost_center(&owner(), &f.scope, "  ").await).subcode,
            "invalid_name"
        );
        // org -> A, repo one -> B, principal writer -> A.
        let a2 = f
            .billing
            .add_cost_center_resources(
                &owner(),
                &f.scope,
                &a.id,
                &[],
                std::slice::from_ref(&f.org.name),
                &["writer".into()],
            )
            .await
            .unwrap();
        assert_eq!(
            (a2.orgs.clone(), a2.principal_ids.clone()),
            (vec![f.org.id.clone()], vec!["writer".to_string()])
        );
        f.billing
            .add_cost_center_resources(&owner(), &f.scope, &b.id, &[repo_ref_of(&r1)], &[], &[])
            .await
            .unwrap();

        let mut in_r1 = hours(&f.org, 1.0);
        in_r1.repo_id = r1.id.clone();
        f.record(in_r1).await; // repo beats org -> B
        let mut in_r2 = hours(&f.org, 2.0);
        in_r2.repo_id = r2.id.clone();
        f.record(in_r2).await; // org -> A
        let mut by_agent = hours(&f.org, 4.0);
        by_agent.principal_id = "agent".into();
        f.record(by_agent).await; // the agent's owner is writer -> A
        let report = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::CostCenter],
                ..Default::default()
            })
            .await;
        let by_cc: Vec<(&str, f64)> = report
            .line_items
            .iter()
            .map(|l| (l.cost_center_id.as_str(), l.quantity))
            .collect();
        let mut expect = vec![(a.id.as_str(), 6.0), (b.id.as_str(), 1.0)];
        expect.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(by_cc, expect);
        let only_b = f
            .query(UsageQuery {
                cost_center_id: b.id.clone(),
                ..Default::default()
            })
            .await;
        assert_eq!(only_b.total_gross_micro_usd, 90_000);
        assert_eq!(
            err(f
                .billing
                .query_usage(
                    &owner(),
                    &f.scope,
                    UsageQuery {
                        cost_center_id: "nope".into(),
                        ..Default::default()
                    }
                )
                .await)
            .reason,
            ErrorReason::NotFound
        );

        // Moving repo one to A takes it out of B.
        let a3 = f
            .billing
            .add_cost_center_resources(&owner(), &f.scope, &a.id, &[repo_ref_of(&r1)], &[], &[])
            .await
            .unwrap();
        assert_eq!(a3.repos.len(), 1);
        let b2 = f
            .billing
            .get_cost_center(&owner(), &f.scope, &b.id)
            .await
            .unwrap();
        assert!(b2.repos.is_empty());
        let moved = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::CostCenter],
                ..Default::default()
            })
            .await;
        assert_eq!(moved.line_items.len(), 1);
        assert_eq!(moved.line_items[0].quantity, 7.0);

        // Rename, list, remove, delete.
        let renamed = f
            .billing
            .update_cost_center(&owner(), &f.scope, &b.id, Some("Growth".into()))
            .await
            .unwrap();
        assert_eq!(renamed.name, "Growth");
        assert_eq!(
            err(f
                .billing
                .update_cost_center(&owner(), &f.scope, &b.id, Some("platform".into()))
                .await)
            .reason,
            ErrorReason::AlreadyExists
        );
        let (list, next) = f
            .billing
            .list_cost_centers(&owner(), &f.scope, "", 1)
            .await
            .unwrap();
        assert_eq!((list.len(), next.is_empty()), (1, false));
        let (rest, _) = f
            .billing
            .list_cost_centers(&owner(), &f.scope, &next, 10)
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        let trimmed = f
            .billing
            .remove_cost_center_resources(
                &owner(),
                &f.scope,
                &a.id,
                &[repo_ref_of(&r1), repo_ref_of(&r2)],
                &[],
                &["writer".into()],
            )
            .await
            .unwrap();
        assert!(trimmed.repos.is_empty() && trimmed.principal_ids.is_empty());
        assert_eq!(trimmed.orgs.len(), 1);
        f.billing
            .delete_cost_center(&owner(), &f.scope, &a.id)
            .await
            .unwrap();
        let reverted = f
            .query(UsageQuery {
                group_by: vec![BillingUsageGrouping::CostCenter],
                ..Default::default()
            })
            .await;
        assert_eq!(reverted.line_items.len(), 1);
        assert_eq!(
            reverted.line_items[0].cost_center_id, "",
            "usage reverts to the account"
        );
        assert_eq!(
            err(f.billing.get_cost_center(&owner(), &f.scope, &a.id).await).reason,
            ErrorReason::NotFound
        );
        // Resources outside the account are refused.
        let other = support::make_org(&f.env, &format!("away{}", f.org.name)).await;
        let foreign = support::make_repo(&f.env, &other, "x").await;
        assert_eq!(
            err(f
                .billing
                .add_cost_center_resources(
                    &owner(),
                    &f.scope,
                    &b.id,
                    &[],
                    std::slice::from_ref(&other.name),
                    &[]
                )
                .await)
            .reason,
            ErrorReason::NotFound
        );
        assert_eq!(
            err(f
                .billing
                .add_cost_center_resources(
                    &owner(),
                    &f.scope,
                    &b.id,
                    &[repo_ref_of(&foreign)],
                    &[],
                    &[]
                )
                .await)
            .reason,
            ErrorReason::NotFound
        );
        assert!(!f.events("billing.cost_center_changed").await.is_empty());
    })
    .await;
}

// ---------- payment method and email ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_payment_method_and_email_settings() {
    on_backends(|store, clock| async move {
        let cfg = BillingConfig {
            portal_url: "https://billing.example/portal".into(),
            ..BillingConfig::default()
        };
        let f = fx_with(store, clock, cfg).await;
        let none = f
            .billing
            .get_payment_method(&owner(), &f.scope)
            .await
            .unwrap();
        assert_eq!(none.kind, BillingPaymentMethodKind::None as i32);
        assert_eq!(none.portal_url, "https://billing.example/portal");
        let card = PaymentMethodInput {
            kind: BillingPaymentMethodKind::Card,
            card_brand: "visa".into(),
            card_last4: "4242".into(),
            card_exp_month: 12,
            card_exp_year: 2030,
            billing_name: "Acme Inc".into(),
            country: "DE".into(),
        };
        f.billing
            .set_payment_method(&owner(), &f.scope, card.clone())
            .await
            .unwrap();
        let got = f
            .billing
            .get_payment_method(&owner(), &f.scope)
            .await
            .unwrap();
        assert_eq!(
            (
                got.card_brand.as_str(),
                got.card_last4.as_str(),
                got.card_exp_year
            ),
            ("visa", "4242", 2030)
        );
        assert!(got.updated_at.is_some());
        for bad in [
            PaymentMethodInput {
                card_last4: "4242424242424242".into(),
                ..card.clone()
            },
            PaymentMethodInput {
                card_exp_month: 13,
                ..card.clone()
            },
            PaymentMethodInput {
                country: "Germany".into(),
                ..card.clone()
            },
            PaymentMethodInput {
                kind: BillingPaymentMethodKind::Invoice,
                ..card.clone()
            },
            PaymentMethodInput {
                kind: BillingPaymentMethodKind::Unspecified,
                ..Default::default()
            },
        ] {
            assert_eq!(
                err(f.billing.set_payment_method(&owner(), &f.scope, bad).await).reason,
                ErrorReason::InvalidArgument
            );
        }
        f.billing
            .set_payment_method(
                &owner(),
                &f.scope,
                PaymentMethodInput {
                    kind: BillingPaymentMethodKind::Invoice,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let inv = f
            .billing
            .get_payment_method(&owner(), &f.scope)
            .await
            .unwrap();
        assert_eq!(
            (inv.kind, inv.card_last4.as_str()),
            (BillingPaymentMethodKind::Invoice as i32, "")
        );

        let d = f
            .billing
            .get_email_settings(&owner(), &f.scope)
            .await
            .unwrap();
        assert!(d.send_receipts && d.send_usage_alerts && d.primary_email.is_empty());
        let s = f
            .billing
            .update_email_settings(
                &owner(),
                &f.scope,
                UpdateBillingEmailSettingsRequest {
                    primary_email: Some("ap@acme.test".into()),
                    additional_emails: vec![
                        "b@acme.test".into(),
                        "a@acme.test".into(),
                        "b@acme.test".into(),
                    ],
                    replace_additional_emails: true,
                    send_usage_alerts: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(s.additional_emails, ["a@acme.test", "b@acme.test"]);
        assert!(s.send_receipts && !s.send_usage_alerts);
        // Unset fields stay.
        let s = f
            .billing
            .update_email_settings(
                &owner(),
                &f.scope,
                UpdateBillingEmailSettingsRequest {
                    send_receipts: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (
                s.primary_email.as_str(),
                s.additional_emails.len(),
                s.send_receipts
            ),
            ("ap@acme.test", 2, false)
        );
        for bad in ["nope", "a@b", "x @y.z"] {
            let e = err(f
                .billing
                .update_email_settings(
                    &owner(),
                    &f.scope,
                    UpdateBillingEmailSettingsRequest {
                        primary_email: Some(bad.into()),
                        ..Default::default()
                    },
                )
                .await);
            assert_eq!(e.subcode, "invalid_email");
        }
        // With usage alerts off, budget alerts do not carry the addresses.
        f.billing
            .create_budget(
                &owner(),
                &f.scope,
                CreateBillingBudgetRequest {
                    name: "b".into(),
                    monthly_amount_micro_usd: 10,
                    alert_threshold_percents: vec![100],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.record(hours(&f.org, 1.0)).await;
        let alerts = f.events("billing.budget_threshold_reached").await;
        let p: serde_json::Value = serde_json::from_str(&alerts[0].payload_json).unwrap();
        assert_eq!(p["emails"], serde_json::json!([]));
        assert!(!f.events("billing.payment_method_updated").await.is_empty());
        assert!(!f.events("billing.email_settings_updated").await.is_empty());
    })
    .await;
}

// ---------- audit ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_changes_are_audited_in_the_org_chain() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        f.billing
            .create_cost_center(&owner(), &f.scope, "eng")
            .await
            .unwrap();
        f.billing
            .change_plan(
                &owner(),
                &f.scope,
                PlanChange {
                    plan_id: "team".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.env.forge.flush().await.unwrap();
        let (entries, _) = f
            .env
            .forge
            .audit
            .query(
                &owner(),
                &bee_forge::audit::org_scope(&f.org.id),
                &Default::default(),
                true,
                "",
                100,
            )
            .await
            .unwrap();
        let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
        assert!(
            actions.contains(&"billing.cost_center.create"),
            "{actions:?}"
        );
        assert!(actions.contains(&"billing.plan.change"), "{actions:?}");
        let e = entries
            .iter()
            .find(|e| e.action == "billing.plan.change")
            .unwrap();
        assert_eq!(e.actor.as_ref().unwrap().id, "owner");
        assert!(e.before.contains("free") && e.after.contains("team"));
    })
    .await;
}

// ---------- BillingService ----------

struct Svc {
    f: Fx,
    svc: BillingService,
}

async fn svc(cfg: BillingConfig) -> Svc {
    let f = fx_with(
        Arc::new(bee_forge::store::memory::MemoryStore::new()),
        backends::clock(),
        cfg,
    )
    .await;
    let resolver: Arc<dyn ScopeResolver> = Arc::new(OrgScopeResolver::new(f.env.forge.clone()));
    let svc = BillingService::new(
        f.billing.clone(),
        resolver,
        ForgeServices::new(f.env.forge.clone()),
    );
    Svc { f, svc }
}

fn reason_of<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>) -> String {
    common::reason(r)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_service_permission_matrix() {
    let s = svc(BillingConfig::default()).await;
    let (f, svc) = (&s.f, &s.svc);
    let scope = scope_msg(&f.org);
    for who in ["owner", "admin", "writer"] {
        let tok = f.env.full(who);
        let plan = svc
            .get_account_billing_plan(common::req(
                &tok,
                GetAccountBillingPlanRequest {
                    scope: scope.clone(),
                },
            ))
            .await
            .unwrap_or_else(|e| panic!("{who} reads: {e:?}"))
            .into_inner();
        assert_eq!(plan.plan.unwrap().id, "free");
        let b = svc
            .create_billing_budget(common::req(
                &tok,
                CreateBillingBudgetRequest {
                    scope: scope.clone(),
                    name: format!("by {who}"),
                    monthly_amount_micro_usd: 1,
                    ..Default::default()
                },
            ))
            .await
            .unwrap_or_else(|e| panic!("{who} writes: {e:?}"))
            .into_inner();
        svc.delete_billing_budget(common::req(
            &tok,
            DeleteBillingBudgetRequest {
                scope: scope.clone(),
                budget_id: b.id,
            },
        ))
        .await
        .unwrap();
    }
    for who in ["reader", "outsider"] {
        let tok = f.env.full(who);
        assert_eq!(
            reason_of(
                svc.get_account_billing_plan(common::req(
                    &tok,
                    GetAccountBillingPlanRequest {
                        scope: scope.clone()
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED",
            "{who} reads"
        );
        assert_eq!(
            reason_of(
                svc.list_invoices(common::req(
                    &tok,
                    ListInvoicesRequest {
                        scope: scope.clone(),
                        ..Default::default()
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
        assert_eq!(
            reason_of(
                svc.create_billing_budget(common::req(
                    &tok,
                    CreateBillingBudgetRequest {
                        scope: scope.clone(),
                        name: "x".into(),
                        ..Default::default()
                    },
                ))
                .await
            ),
            "PERMISSION_DENIED",
            "{who} writes"
        );
        assert_eq!(
            reason_of(
                svc.change_billing_plan(common::req(
                    &tok,
                    ChangeBillingPlanRequest {
                        scope: scope.clone(),
                        plan_id: "team".into(),
                        ..Default::default()
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
    }
    // The denied writes are audited in the org's chain.
    f.env.forge.flush().await.unwrap();
    let (entries, _) = f
        .env
        .forge
        .audit
        .query(
            &owner(),
            &bee_forge::audit::org_scope(&f.org.id),
            &Default::default(),
            true,
            "",
            200,
        )
        .await
        .unwrap();
    let denied: Vec<_> = entries
        .iter()
        .filter(|e| e.result == "PERMISSION_DENIED")
        .collect();
    assert!(denied.iter().any(|e| e.action == "billing.budget.create"));
    assert!(denied.iter().any(|e| e.action == "billing.plan.change"));
    // No token, no access; a token without the manage action cannot write.
    let anon = tonic::Request::new(GetAccountBillingPlanRequest {
        scope: scope.clone(),
    });
    assert_eq!(
        reason_of(svc.get_account_billing_plan(anon).await),
        "UNAUTHENTICATED"
    );
    let read_only = f
        .env
        .token("owner", common::actions(&[bee_policy::Action::Read]));
    assert_eq!(
        reason_of(
            svc.create_billing_budget(common::req(
                &read_only,
                CreateBillingBudgetRequest {
                    scope: scope.clone(),
                    name: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "POLICY_DENIED"
    );
    // An enterprise scope needs an enterprise-aware resolver.
    let ent = Some(EnterpriseScope {
        scope: Some(enterprise_scope::Scope::Enterprise("globex".into())),
    });
    let tok = f.env.full("owner");
    assert_eq!(
        reason_of(
            svc.get_account_billing_plan(common::req(
                &tok,
                GetAccountBillingPlanRequest { scope: ent }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason_of(
            svc.get_account_billing_plan(common::req(
                &tok,
                GetAccountBillingPlanRequest { scope: None }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_service_serves_all_twenty_two_rpcs() {
    let s = svc(BillingConfig::default()).await;
    let (f, svc) = (&s.f, &s.svc);
    let tok = f.env.full("writer"); // a billing manager can do all of it
    let scope = scope_msg(&f.org);
    let repo = f.repo("app").await;

    let plans = svc
        .list_billing_plans(common::req(
            &tok,
            ListBillingPlansRequest {
                scope: scope.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        plans.plans.len(),
        5,
        "free, team, enterprise and the two programs"
    );
    let p = svc
        .change_billing_plan(common::req(
            &tok,
            ChangeBillingPlanRequest {
                scope: scope.clone(),
                plan_id: "team".into(),
                seats: 6,
                cycle: BillingCycle::Monthly as i32,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.seats_purchased, 6);
    let seats = svc
        .list_billing_seats(common::req(
            &tok,
            ListBillingSeatsRequest {
                scope: scope.clone(),
                cost_center_id: String::new(),
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: String::new(),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((seats.seats.len(), seats.total_consumed), (2, 3));
    assert!(!seats.page.unwrap().next_page_token.is_empty());

    f.record(input(
        &f.org,
        BillingProduct::ActionsMinutes,
        "actions_linux_2core",
        3500.0,
        "minute",
    ))
    .await;
    let usage = svc
        .query_billing_usage(common::req(
            &tok,
            QueryBillingUsageRequest {
                scope: scope.clone(),
                products: vec![BillingProduct::ActionsMinutes as i32],
                group_by: vec![BillingUsageGrouping::Sku as i32],
                repo: Some(repo_ref_of(&repo)),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(usage.line_items.is_empty() && usage.data_complete_until.is_some());
    let usage = svc
        .query_billing_usage(common::req(
            &tok,
            QueryBillingUsageRequest {
                scope: scope.clone(),
                group_by: vec![BillingUsageGrouping::Sku as i32],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (usage.line_items.len(), usage.total_gross_micro_usd),
        (1, 28_000_000)
    );

    let b = svc
        .create_billing_budget(common::req(
            &tok,
            CreateBillingBudgetRequest {
                scope: scope.clone(),
                name: "b".into(),
                monthly_amount_micro_usd: 100,
                hard_stop: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(b.scope, scope);
    let got = svc
        .get_billing_budget(common::req(
            &tok,
            GetBillingBudgetRequest {
                scope: scope.clone(),
                budget_id: b.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.spent_micro_usd, 4_000_000);
    let upd = svc
        .update_billing_budget(common::req(
            &tok,
            UpdateBillingBudgetRequest {
                scope: scope.clone(),
                budget_id: b.id.clone(),
                name: Some("c".into()),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(upd.name, "c");
    let listed = svc
        .list_billing_budgets(common::req(
            &tok,
            ListBillingBudgetsRequest {
                scope: scope.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.budgets.len(), 1);
    svc.delete_billing_budget(common::req(
        &tok,
        DeleteBillingBudgetRequest {
            scope: scope.clone(),
            budget_id: b.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason_of(
            svc.get_billing_budget(common::req(
                &tok,
                GetBillingBudgetRequest {
                    scope: scope.clone(),
                    budget_id: b.id
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );

    let now = T0 + 20 * DAY;
    f.clock.set(now);
    let tok = f.env.full("writer"); // tokens minted before the clock moved are expired
    let made = f
        .billing
        .close_cycle(&owner(), &f.scope, now)
        .await
        .unwrap();
    let list = svc
        .list_invoices(common::req(
            &tok,
            ListInvoicesRequest {
                scope: scope.clone(),
                status: InvoiceStatus::Open as i32,
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.invoices.len(), 1);
    let one = svc
        .get_invoice(common::req(
            &tok,
            GetInvoiceRequest {
                scope: scope.clone(),
                invoice_id: made[0].id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(one.invoice.unwrap().line_items.len() >= 2);
    assert!(one.pdf_url.contains(".pdf?exp=") && one.pdf_url_expires_at.is_some());

    let cc = svc
        .create_cost_center(common::req(
            &tok,
            CreateCostCenterRequest {
                scope: scope.clone(),
                name: "eng".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let cc = svc
        .add_cost_center_resources(common::req(
            &tok,
            AddCostCenterResourcesRequest {
                scope: scope.clone(),
                cost_center_id: cc.id.clone(),
                repos: vec![repo_ref_of(&repo)],
                orgs: vec![],
                principal_ids: vec!["reader".into()],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((cc.repos.len(), cc.principal_ids.len()), (1, 1));
    let cc = svc
        .remove_cost_center_resources(common::req(
            &tok,
            RemoveCostCenterResourcesRequest {
                scope: scope.clone(),
                cost_center_id: cc.id.clone(),
                repos: vec![repo_ref_of(&repo)],
                orgs: vec![],
                principal_ids: vec![],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(cc.repos.is_empty());
    let cc = svc
        .update_cost_center(common::req(
            &tok,
            UpdateCostCenterRequest {
                scope: scope.clone(),
                cost_center_id: cc.id.clone(),
                name: Some("platform".into()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cc.name, "platform");
    let got = svc
        .get_cost_center(common::req(
            &tok,
            GetCostCenterRequest {
                scope: scope.clone(),
                cost_center_id: cc.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.id, cc.id);
    let all = svc
        .list_cost_centers(common::req(
            &tok,
            ListCostCentersRequest {
                scope: scope.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.cost_centers.len(), 1);
    svc.delete_cost_center(common::req(
        &tok,
        DeleteCostCenterRequest {
            scope: scope.clone(),
            cost_center_id: cc.id,
        },
    ))
    .await
    .unwrap();

    let pm = svc
        .get_billing_payment_method(common::req(
            &tok,
            GetBillingPaymentMethodRequest {
                scope: scope.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(pm.kind, BillingPaymentMethodKind::None as i32);
    let email = svc
        .get_billing_email_settings(common::req(
            &tok,
            GetBillingEmailSettingsRequest {
                scope: scope.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(email.send_receipts);
    let email = svc
        .update_billing_email_settings(common::req(
            &tok,
            UpdateBillingEmailSettingsRequest {
                scope: scope.clone(),
                primary_email: Some("a@b.co".into()),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(email.primary_email, "a@b.co");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn billing_service_answers_unimplemented_when_billing_is_disabled() {
    let s = svc(BillingConfig {
        enabled: false,
        ..BillingConfig::default()
    })
    .await;
    let (f, svc) = (&s.f, &s.svc);
    let tok = f.env.full("owner");
    let scope = scope_msg(&f.org);
    let plan = svc
        .get_account_billing_plan(common::req(
            &tok,
            GetAccountBillingPlanRequest {
                scope: scope.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(plan.billing_disabled);
    let st = svc
        .change_billing_plan(common::req(
            &tok,
            ChangeBillingPlanRequest {
                scope: scope.clone(),
                plan_id: "team".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(st.code(), tonic::Code::Unimplemented);
    let d = bee_proto::errors::detail_from_status(&st).unwrap();
    assert_eq!(
        (d.reason.as_str(), d.subcode.as_str()),
        ("UNIMPLEMENTED", "billing_disabled")
    );
    assert_eq!(
        reason_of(
            svc.list_invoices(common::req(
                &tok,
                ListInvoicesRequest {
                    scope: scope.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "UNIMPLEMENTED"
    );
    svc.list_billing_budgets(common::req(
        &tok,
        ListBillingBudgetsRequest {
            scope,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
}

#[test]
fn billing_registry_is_consistent() {
    let r = bee_forge::module::Registry::core().unwrap();
    for k in [
        "usage.recorded",
        "billing.budget_threshold_reached",
        "billing.invoice_created",
    ] {
        assert!(r.event_kind(k).is_some(), "{k}");
    }
    assert!(r.surfaces().iter().any(|s| s.name() == "billing"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn education_and_nonprofit_programs_are_discounts_a_program_admin_assigns() {
    on_backends(|store, clock| async move {
        let cfg = BillingConfig {
            program_admins: ["siteadmin".to_string()].into_iter().collect(),
            ..BillingConfig::default()
        };
        let f = fx_with(store, clock, cfg).await;
        let change = |plan: &str| PlanChange {
            plan_id: plan.into(),
            seats: 0,
            cycle: BillingCycle::Unspecified,
        };
        let catalog = f.billing.list_plans(&f.scope);
        let plan = |id: &str| catalog.iter().find(|p| p.id == id).cloned().unwrap();
        let (team, edu, npo) = (plan("team"), plan("education"), plan("nonprofit"));
        // Both are Team at a discount: same features and allowances, lower seat price.
        for p in [&edu, &npo] {
            assert_eq!(p.features, team.features, "{}", p.id);
            assert_eq!(p.included, team.included, "{}", p.id);
            assert!(!p.self_serve, "{} needs eligibility checked first", p.id);
        }
        assert_eq!(edu.seat_monthly_price_micro_usd, 0);
        assert_eq!(
            npo.seat_monthly_price_micro_usd,
            team.seat_monthly_price_micro_usd / 2
        );
        // An org admin cannot pick a program for themselves.
        for id in ["education", "nonprofit"] {
            let e = err(f.billing.change_plan(&owner(), &f.scope, change(id)).await);
            assert_eq!(
                (e.reason, e.subcode.as_str()),
                (ErrorReason::InvalidState, "contact_sales")
            );
        }
        // A program administrator assigns it once eligibility is established.
        let admin = support::cx("siteadmin");
        let p = f
            .billing
            .change_plan(&admin, &f.scope, change("education"))
            .await
            .unwrap();
        assert_eq!(p.plan.as_ref().unwrap().id, "education");
        assert_eq!(p.seats_purchased, 0, "no seats are bought on a free plan");
        // Team's allowance applies: 2,500 minutes are covered (Free covers 2,000).
        f.record(input(
            &f.org,
            BillingProduct::ActionsMinutes,
            "actions_linux_2core",
            2500.0,
            "minute",
        ))
        .await;
        let r = f.query(UsageQuery::default()).await;
        assert_eq!(r.total_gross_micro_usd, 20_000_000);
        assert_eq!(r.total_net_micro_usd, 0, "fully discounted");
        // Nonprofit: seats at half of Team's price.
        let p = f
            .billing
            .change_plan(&admin, &f.scope, change("nonprofit"))
            .await
            .unwrap();
        assert_eq!(p.plan.as_ref().unwrap().id, "nonprofit");
        assert_eq!(p.seats_purchased, p.seats_consumed.max(1));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_budget_creates_respect_the_cap_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = bee_forge::store::sql::SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_billing_budget_cap_{}", std::process::id());
    }
    let first_store: Arc<dyn ForgeStore> = Arc::new(
        bee_forge::store::sql::SqlStore::open(cfg.clone(), Arc::new(bee_forge::ids::SystemClock))
            .await
            .unwrap(),
    );
    let second_store: Arc<dyn ForgeStore> = Arc::new(
        bee_forge::store::sql::SqlStore::open(cfg, Arc::new(bee_forge::ids::SystemClock))
            .await
            .unwrap(),
    );
    let clock = Arc::new(bee_forge::ids::ManualClock::new(T0));
    let first = fx(first_store, clock.clone()).await;
    let second_env = common::env_with(second_store, clock.clone()).await;
    let second_billing = Billing::new(second_env.forge.clone(), BillingConfig::default());
    let second_scope = OrgScopeResolver::new(second_env.forge.clone())
        .resolve_org(&owner(), &first.org.name)
        .await
        .unwrap();

    // MAX_BUDGETS is crate-private; keep this aligned with its configured value.
    let cap = 500;
    let mut existing_budget_id = None;
    for i in 0..cap - 1 {
        let mut req = first.hard_stop_budget(1_000_000, WS);
        req.name = format!("seed_{i}");
        let created = first
            .billing
            .create_budget(&owner(), &first.scope, req)
            .await
            .unwrap();
        if i == 0 {
            existing_budget_id = Some(created.id);
        }
    }
    first
        .billing
        .update_budget(
            &owner(),
            &first.scope,
            UpdateBillingBudgetRequest {
                budget_id: existing_budget_id.unwrap(),
                name: Some("seed_0_updated".into()),
                ..Default::default()
            },
        )
        .await
        .expect("updating an existing budget at the account cap remains allowed");

    for i in 0..24 {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left_billing = first.billing.clone();
        let right_billing = second_billing.clone();
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left_scope = first.scope.clone();
        let right_scope = second_scope.clone();
        let left_cx = owner();
        let right_cx = owner();
        let mut left_req = first.hard_stop_budget(1_000_000, WS);
        left_req.name = format!("race_{i}_left");
        let mut right_req = first.hard_stop_budget(1_000_000, WS);
        right_req.name = format!("race_{i}_right");
        let (left, right) = tokio::join!(
            async move {
                left_barrier.wait().await;
                left_billing
                    .create_budget(&left_cx, &left_scope, left_req)
                    .await
            },
            async move {
                right_barrier.wait().await;
                right_billing
                    .create_budget(&right_cx, &right_scope, right_req)
                    .await
            }
        );
        assert!(
            left.is_ok() ^ right.is_ok(),
            "exactly one budget insert must succeed at round {i}: {left:?} {right:?}"
        );
        let loser = left.as_ref().err().or(right.as_ref().err()).unwrap();
        assert_eq!(loser.reason, ErrorReason::TooLarge);
        assert_eq!(
            loser.message,
            format!("budgets is {}, above the limit of {cap}", cap + 1)
        );
        let (budgets, _) = first
            .billing
            .list_budgets(
                &owner(),
                &first.scope,
                BillingProduct::Unspecified,
                "",
                cap + 100,
            )
            .await
            .unwrap();
        assert_eq!(budgets.len(), cap);
        let winner_id = left
            .as_ref()
            .ok()
            .or(right.as_ref().ok())
            .unwrap()
            .id
            .clone();
        first
            .billing
            .delete_budget(&owner(), &first.scope, &winner_id)
            .await
            .unwrap();
    }
}
