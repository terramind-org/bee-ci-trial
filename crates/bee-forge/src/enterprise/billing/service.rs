//! `BillingService`.
//!
//! Every RPC resolves its `EnterpriseScope` through the [`ScopeResolver`] and authorizes the
//! caller with [`ScopeNeed::Billing`] (org owners, admins and billing managers; enterprise
//! owners through the enterprise resolver) for reads and writes alike. Denied state-changing
//! calls are audited. Org names in answers are ids (see `docs/spec/orgs-apps.md`).

use std::sync::Arc;

use bee_proto::v1::billing_service_server::{BillingService as BillingRpc, BillingServiceServer};
use bee_proto::v1::{
    AddCostCenterResourcesRequest, BillingAccountPlan, BillingBudget, BillingEmailSettings,
    BillingPaymentMethod, BillingProduct, BillingUsageGrouping, ChangeBillingPlanRequest,
    CostCenter, CreateBillingBudgetRequest, CreateCostCenterRequest, DeleteBillingBudgetRequest,
    DeleteBillingBudgetResponse, DeleteCostCenterRequest, DeleteCostCenterResponse,
    EnterpriseScope, ErrorReason, GetAccountBillingPlanRequest, GetBillingBudgetRequest,
    GetBillingEmailSettingsRequest, GetBillingPaymentMethodRequest, GetCostCenterRequest,
    GetInvoiceRequest, GetInvoiceResponse, InvoiceStatus, ListBillingBudgetsRequest,
    ListBillingBudgetsResponse, ListBillingPlansRequest, ListBillingPlansResponse,
    ListBillingSeatsRequest, ListBillingSeatsResponse, ListCostCentersRequest,
    ListCostCentersResponse, ListInvoicesRequest, ListInvoicesResponse, QueryBillingUsageRequest,
    QueryBillingUsageResponse, RemoveCostCenterResourcesRequest, UpdateBillingBudgetRequest,
    UpdateBillingEmailSettingsRequest, UpdateCostCenterRequest,
};
use tonic::{Request, Response, Status};

use super::usage::UsageQuery;
use super::{Billing, PlanChange};
use crate::auth::Caller;
use crate::cx::Cx;
use crate::enterprise::common::{ResolvedScope, ScopeNeed, ScopeResolver, page_args};
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::service::{ForgeServices, next, reply};

/// `BillingService` over a [`Billing`].
#[derive(Clone)]
pub struct BillingService {
    billing: Arc<Billing>,
    resolver: Arc<dyn ScopeResolver>,
    services: ForgeServices,
}

impl BillingService {
    /// The service. The resolver is also installed in `billing` so budgets and limits see
    /// enterprise accounts the same way the RPCs do.
    pub fn new(
        billing: Arc<Billing>,
        resolver: Arc<dyn ScopeResolver>,
        services: ForgeServices,
    ) -> BillingService {
        billing.set_scope_resolver(resolver.clone());
        BillingService {
            billing,
            resolver,
            services,
        }
    }

    /// The tonic server to register.
    pub fn server(
        billing: Arc<Billing>,
        resolver: Arc<dyn ScopeResolver>,
        services: ForgeServices,
    ) -> BillingServiceServer<BillingService> {
        BillingServiceServer::new(BillingService::new(billing, resolver, services))
    }

    /// Resolves the scope and authorizes the caller for billing access. For state-changing
    /// calls (`audited`), a denial is recorded in the audit log under `action`.
    async fn scoped(
        &self,
        caller: &Caller,
        scope: &Option<EnterpriseScope>,
        action: &str,
        audited: bool,
    ) -> Result<(ResolvedScope, Cx)> {
        let rs = self.resolver.resolve(&caller.cx, scope).await?;
        let mut auth = self
            .resolver
            .authorize(caller, &rs, ScopeNeed::Billing)
            .await;
        // The billing-manager path of the resolver checks roles and the `read` action only;
        // changes also need a token that may manage settings.
        if audited
            && auth.is_ok()
            && !caller
                .token
                .allows_action(bee_policy::Action::ManageSettings)
        {
            auth = Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "token does not allow `manage_settings`",
            ));
        }
        let cx = if audited {
            self.services
                .guarded(caller, &rs.audit_org(), "", action, auth)
                .await?
        } else {
            auth?
        };
        Ok((rs, cx))
    }
}

fn opt_ms(t: &Option<pbjson_types::Timestamp>) -> Option<u64> {
    t.as_ref().map(ts_ms)
}

#[tonic::async_trait]
impl BillingRpc for BillingService {
    async fn list_billing_plans(
        &self,
        req: Request<ListBillingPlansRequest>,
    ) -> std::result::Result<Response<ListBillingPlansResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, _) = self.scoped(&caller, &r.scope, "", false).await?;
            Ok(ListBillingPlansResponse {
                plans: self.billing.list_plans(&scope),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_account_billing_plan(
        &self,
        req: Request<GetAccountBillingPlanRequest>,
    ) -> std::result::Result<Response<BillingAccountPlan>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            self.billing.account_plan(&cx, &scope).await
        }
        .await;
        reply(&caller, out)
    }

    async fn change_billing_plan(
        &self,
        req: Request<ChangeBillingPlanRequest>,
    ) -> std::result::Result<Response<BillingAccountPlan>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.plan.change", true)
                .await?;
            self.billing
                .change_plan(
                    &cx,
                    &scope,
                    PlanChange {
                        plan_id: r.plan_id,
                        seats: r.seats,
                        cycle: bee_proto::v1::BillingCycle::try_from(r.cycle).unwrap_or_default(),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_billing_seats(
        &self,
        req: Request<ListBillingSeatsRequest>,
    ) -> std::result::Result<Response<ListBillingSeatsResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            let (token, size) = page_args(&r.page);
            let (seats, total, token) = self
                .billing
                .list_seats(&cx, &scope, &r.cost_center_id, &token, size)
                .await?;
            Ok(ListBillingSeatsResponse {
                seats,
                total_consumed: total,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn query_billing_usage(
        &self,
        req: Request<QueryBillingUsageRequest>,
    ) -> std::result::Result<Response<QueryBillingUsageResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            let (token, size) = page_args(&r.page);
            let report = self
                .billing
                .query_usage(
                    &cx,
                    &scope,
                    UsageQuery {
                        products: r
                            .products
                            .iter()
                            .filter_map(|p| BillingProduct::try_from(*p).ok())
                            .collect(),
                        start_ms: opt_ms(&r.start_at),
                        end_ms: opt_ms(&r.end_at),
                        group_by: r
                            .group_by
                            .iter()
                            .filter_map(|g| BillingUsageGrouping::try_from(*g).ok())
                            .collect(),
                        repo: r.repo,
                        cost_center_id: r.cost_center_id,
                        org: r.org,
                        after: token,
                        limit: size,
                    },
                )
                .await?;
            Ok(QueryBillingUsageResponse {
                line_items: report.line_items,
                total_gross_micro_usd: report.total_gross_micro_usd,
                total_net_micro_usd: report.total_net_micro_usd,
                data_complete_until: Some(ts(report.data_complete_until_ms)),
                page: next(report.next_token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_billing_budget(
        &self,
        req: Request<CreateBillingBudgetRequest>,
    ) -> std::result::Result<Response<BillingBudget>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.budget.create", true)
                .await?;
            self.billing.create_budget(&cx, &scope, r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_billing_budget(
        &self,
        req: Request<GetBillingBudgetRequest>,
    ) -> std::result::Result<Response<BillingBudget>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            self.billing.get_budget(&cx, &scope, &r.budget_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_billing_budget(
        &self,
        req: Request<UpdateBillingBudgetRequest>,
    ) -> std::result::Result<Response<BillingBudget>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.budget.update", true)
                .await?;
            self.billing.update_budget(&cx, &scope, r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_billing_budget(
        &self,
        req: Request<DeleteBillingBudgetRequest>,
    ) -> std::result::Result<Response<DeleteBillingBudgetResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.budget.delete", true)
                .await?;
            self.billing
                .delete_budget(&cx, &scope, &r.budget_id)
                .await?;
            Ok(DeleteBillingBudgetResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_billing_budgets(
        &self,
        req: Request<ListBillingBudgetsRequest>,
    ) -> std::result::Result<Response<ListBillingBudgetsResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            let (token, size) = page_args(&r.page);
            let product = BillingProduct::try_from(r.product).unwrap_or_default();
            let (budgets, token) = self
                .billing
                .list_budgets(&cx, &scope, product, &token, size)
                .await?;
            Ok(ListBillingBudgetsResponse {
                budgets,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_invoices(
        &self,
        req: Request<ListInvoicesRequest>,
    ) -> std::result::Result<Response<ListInvoicesResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            let (token, size) = page_args(&r.page);
            let status = InvoiceStatus::try_from(r.status).unwrap_or_default();
            let (invoices, token) = self
                .billing
                .list_invoices(&cx, &scope, status, &token, size)
                .await?;
            Ok(ListInvoicesResponse {
                invoices,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_invoice(
        &self,
        req: Request<GetInvoiceRequest>,
    ) -> std::result::Result<Response<GetInvoiceResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            let (invoice, url) = self.billing.get_invoice(&cx, &scope, &r.invoice_id).await?;
            Ok(GetInvoiceResponse {
                invoice: Some(invoice),
                pdf_url: url.url,
                pdf_url_expires_at: Some(ts(url.expires_at_ms)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_cost_center(
        &self,
        req: Request<CreateCostCenterRequest>,
    ) -> std::result::Result<Response<CostCenter>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.cost_center.create", true)
                .await?;
            self.billing.create_cost_center(&cx, &scope, &r.name).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_cost_center(
        &self,
        req: Request<GetCostCenterRequest>,
    ) -> std::result::Result<Response<CostCenter>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            self.billing
                .get_cost_center(&cx, &scope, &r.cost_center_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_cost_center(
        &self,
        req: Request<UpdateCostCenterRequest>,
    ) -> std::result::Result<Response<CostCenter>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.cost_center.update", true)
                .await?;
            self.billing
                .update_cost_center(&cx, &scope, &r.cost_center_id, r.name)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_cost_center(
        &self,
        req: Request<DeleteCostCenterRequest>,
    ) -> std::result::Result<Response<DeleteCostCenterResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.cost_center.delete", true)
                .await?;
            self.billing
                .delete_cost_center(&cx, &scope, &r.cost_center_id)
                .await?;
            Ok(DeleteCostCenterResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_cost_centers(
        &self,
        req: Request<ListCostCentersRequest>,
    ) -> std::result::Result<Response<ListCostCentersResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            let (token, size) = page_args(&r.page);
            let (cost_centers, token) = self
                .billing
                .list_cost_centers(&cx, &scope, &token, size)
                .await?;
            Ok(ListCostCentersResponse {
                cost_centers,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn add_cost_center_resources(
        &self,
        req: Request<AddCostCenterResourcesRequest>,
    ) -> std::result::Result<Response<CostCenter>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.cost_center.add_resources", true)
                .await?;
            self.billing
                .add_cost_center_resources(
                    &cx,
                    &scope,
                    &r.cost_center_id,
                    &r.repos,
                    &r.orgs,
                    &r.principal_ids,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_cost_center_resources(
        &self,
        req: Request<RemoveCostCenterResourcesRequest>,
    ) -> std::result::Result<Response<CostCenter>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(
                    &caller,
                    &r.scope,
                    "billing.cost_center.remove_resources",
                    true,
                )
                .await?;
            self.billing
                .remove_cost_center_resources(
                    &cx,
                    &scope,
                    &r.cost_center_id,
                    &r.repos,
                    &r.orgs,
                    &r.principal_ids,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_billing_payment_method(
        &self,
        req: Request<GetBillingPaymentMethodRequest>,
    ) -> std::result::Result<Response<BillingPaymentMethod>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            self.billing.get_payment_method(&cx, &scope).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_billing_email_settings(
        &self,
        req: Request<GetBillingEmailSettingsRequest>,
    ) -> std::result::Result<Response<BillingEmailSettings>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self.scoped(&caller, &r.scope, "", false).await?;
            self.billing.get_email_settings(&cx, &scope).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_billing_email_settings(
        &self,
        req: Request<UpdateBillingEmailSettingsRequest>,
    ) -> std::result::Result<Response<BillingEmailSettings>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (scope, cx) = self
                .scoped(&caller, &r.scope, "billing.email_settings.update", true)
                .await?;
            self.billing.update_email_settings(&cx, &scope, r).await
        }
        .await;
        reply(&caller, out)
    }
}
