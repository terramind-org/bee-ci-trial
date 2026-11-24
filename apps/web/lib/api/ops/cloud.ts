/** Billing (billing.proto): plans, the account's plan, seats and usage. */
import { BillingService } from "../../gen/bee/v1/billing_pb";
import { pass, type PartialOps } from "./shared";

export const cloudOps = {
  BillingService: {
    ListBillingPlans: pass(BillingService.method.listBillingPlans),
    GetAccountPlan: pass(BillingService.method.getAccountBillingPlan, (r) => ({ scope: { org: r.org } })),
    ListSeats: pass(BillingService.method.listBillingSeats, (r) => ({ scope: { org: r.org }, page: r.page })),
    QueryUsage: pass(BillingService.method.queryBillingUsage, (r) => ({ scope: { org: r.org }, startAt: r.startAt, endAt: r.endAt, groupBy: r.groupBy, products: r.products, page: r.page })),
    ChangePlan: pass(BillingService.method.changeBillingPlan, (r) => ({ scope: { org: r.org }, planId: r.planId, seats: r.seats, cycle: r.cycle })),
  },
} satisfies PartialOps<"BillingService">;
