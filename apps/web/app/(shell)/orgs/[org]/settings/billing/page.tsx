import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { PageMenu } from "@/components/menu";
import { PrincipalName } from "@/components/domain";
import { loadSection } from "@/components/home/section";
import { PRODUCT_LABEL } from "@/components/marketing/pricing";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader, Panel } from "@/components/ui";
import { edition, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { AccountPlan, BillingPlan } from "@/lib/api/types";
import { guard } from "@/lib/page";
import { plural, usd } from "@/lib/format";
import { SettingsTabs } from "../tabs";
import { ChangePlan } from "./change-plan";

export const metadata: Metadata = { title: "Billing" };
export const dynamic = "force-dynamic";

const num = (n: number) => n.toLocaleString("en-US", { maximumFractionDigits: 1 });

/** `/orgs/{org}/settings/billing`: plan, seats, this cycle's usage and changing plan. Hosted edition only. */
export default async function BillingPage({ params }: { params: Promise<{ org: string }> }) {
  if ((await edition()) !== "cloud") notFound();
  const org = decodeURIComponent((await params).org);
  const op = { service: "BillingService", method: "GetAccountPlan", request: { org }, label: "Show the plan" } as const;
  return guard(async () => {
    const api = await serverClient();
    let account: AccountPlan;
    let plans: BillingPlan[];
    try {
      [account, plans] = await Promise.all([api.billing.getAccountPlan({ org }), api.billing.listBillingPlans({ scope: { org } }).then((r) => r.plans)]);
    } catch (err) {
      if (err instanceof BeeError && err.code === "unimplemented") {
        return (
          <>
            <PageHeader title="Settings" lede={`Plan, seats and usage for ${org}.`} />
            <SettingsTabs org={org} active="billing" />
            <div data-testid="billing-unavailable">
              <EmptyState title="Billing is not available on this server" icon="releases">
                This server does not offer billing (<span className="mono">{err.reason}</span>). {err.message}
              </EmptyState>
            </div>
          </>
        );
      }
      throw err;
    }
    return (
      <>
        <PageHeader title="Settings" lede={<>Plan, seats and usage for {org}.</>} actions={<PageMenu op={op} />} />
        <SettingsTabs org={org} active="billing" />
        <div className="flex flex-col gap-10">
          <PlanPanel account={account} />
          {account.billingDisabled ? null : (
            <Panel title="Change plan" id="change-plan">
              <ChangePlan
                org={org}
                plans={plans.filter((p) => p.selfServe).map((p) => ({ id: p.id, name: p.name, monthlyMicroUsd: Number(p.seatMonthlyPriceMicroUsd), yearlyMicroUsd: Number(p.seatYearlyPriceMicroUsd) }))}
                currentPlan={account.plan?.id ?? ""}
                currentCycle={account.cycle}
                currentSeats={account.seatsPurchased}
              />
            </Panel>
          )}
          <div className="grid items-start gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)] lg:gap-10">
            {await usageSection(org, account)}
            {await seatsSection(org)}
          </div>
        </div>
      </>
    );
  });
}

function PlanPanel({ account }: { account: AccountPlan }) {
  const plan = account.plan;
  return (
    <Panel title="Current plan" id="current-plan">
      <dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 py-3 text-base sm:grid-cols-[auto_1fr_auto_1fr]" data-testid="current-plan">
        <dt className="text-ink-3">Plan</dt>
        <dd className="font-medium">{plan?.name ?? "None"}</dd>
        <dt className="text-ink-3">Billing cycle</dt>
        <dd>{account.cycle === "BILLING_CYCLE_YEARLY" ? "Yearly" : account.cycle === "BILLING_CYCLE_MONTHLY" ? "Monthly" : "Not set"}</dd>
        <dt className="text-ink-3">Seats</dt>
        <dd className="num">
          {account.seatsConsumed} in use of {account.seatsPurchased} purchased
        </dd>
        <dt className="text-ink-3">Next bill</dt>
        <dd>{account.nextBillingAt ? <Time iso={account.nextBillingAt} /> : "None scheduled"}</dd>
      </dl>
      {account.billingDisabled ? <p className="border-t border-line py-3 text-base text-ink-2">Billing is off for this account: nothing is charged.</p> : null}
      {account.pendingPlanId ? (
        <p className="border-t border-line py-3 text-base text-ink-2" data-testid="pending-change">
          At the end of this cycle the account moves to <span className="font-medium text-ink">{account.pendingPlanId}</span> with {plural(account.pendingSeats, "seat")}.
        </p>
      ) : null}
    </Panel>
  );
}

async function usageSection(org: string, account: AccountPlan) {
  return loadSection(
    "Usage",
    async () => {
      const req = { org, startAt: account.cycleStartedAt, endAt: account.nextBillingAt, groupBy: ["BILLING_USAGE_GROUPING_PRODUCT" as const] };
      return { req, res: await (await serverClient()).billing.queryUsage(req) };
    },
    ({ res }) => {
      const rows = res.lineItems.filter((l) => l.product !== "BILLING_PRODUCT_SEATS");
      const max = Math.max(1, ...rows.map((l) => Number(l.netMicroUsd)));
      return (
        <Panel title="Usage this cycle" id="usage">
          {rows.length ? (
            <div className="overflow-x-auto">
              <table className="w-full text-base" data-testid="usage-table">
                <caption className="sr-only">Metered usage since the cycle started, by product</caption>
                <thead>
                  <tr className="border-b border-line text-left text-sm text-ink-3">
                    <th scope="col" className="py-3 font-medium">
                      Product
                    </th>
                    <th scope="col" className="px-2 py-2 text-right font-medium">
                      Used
                    </th>
                    <th scope="col" className="px-2 py-2 text-right font-medium">
                      Charged
                    </th>
                    <th scope="col" className="w-24 py-3 font-medium">
                      <span className="sr-only">Share of the charge</span>
                    </th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-line">
                  {rows.map((l) => (
                    <tr key={l.product + l.sku + l.date}>
                      <th scope="row" className="py-3 text-left font-normal">
                        {PRODUCT_LABEL[l.product] ?? l.product}
                      </th>
                      <td className="num px-2 py-2 text-right text-ink-2">
                        {num(l.quantity)} {l.unit}
                      </td>
                      <td className="num px-2 py-2 text-right">{usd(l.netMicroUsd)}</td>
                      <td className="py-3">
                        <span className="block h-1.5 rounded-full bg-sunken" aria-hidden="true">
                          <span className="block h-full rounded-full bg-ink-3" style={{ width: Math.max(2, Math.round((Number(l.netMicroUsd) / max) * 100)) + "%" }} />
                        </span>
                      </td>
                    </tr>
                  ))}
                </tbody>
                <tfoot>
                  <tr className="border-t border-line">
                    <th scope="row" className="py-3 text-left font-medium">
                      Total
                    </th>
                    <td />
                    <td className="num px-2 py-2 text-right font-medium">{usd(res.totalNetMicroUsd)}</td>
                    <td />
                  </tr>
                </tfoot>
              </table>
            </div>
          ) : (
            <p className=" py-8 text-center text-base text-ink-3">No usage has been metered this cycle.</p>
          )}
          {res.dataCompleteUntil ? (
            <p className="border-t border-line py-3 text-sm text-ink-3">
              Complete until <Time iso={res.dataCompleteUntil} />; later usage may still arrive.
            </p>
          ) : null}
        </Panel>
      );
    },
  );
}

async function seatsSection(org: string) {
  return loadSection(
    "Seats",
    async () => ({ req: { org }, res: await (await serverClient()).billing.listSeats({ org }) }),
    ({ res }) => (
      <Panel
        title="Seats"
        id="seats"
        actions={<span className="num text-base text-ink-3">{res.totalConsumed} in use</span>}
      >
        {res.seats.length ? (
          <ul className="divide-y divide-line" data-testid="seats">
            {res.seats.map((s) => (
              <li key={s.principal?.id ?? s.org} className="flex min-h-11 items-center gap-2.5 py-3 text-base">
                <span className="min-w-0 flex-1">
                  <PrincipalName principal={s.principal} />
                </span>
                {s.outsideCollaborator ? <Badge tone="outline">outside collaborator</Badge> : null}
                {s.lastActivityAt ? <Time iso={s.lastActivityAt} className="num shrink-0 text-sm text-ink-3" /> : null}
              </li>
            ))}
          </ul>
        ) : (
          <p className=" py-8 text-center text-base text-ink-3">No seats are in use.</p>
        )}
      </Panel>
    ),
  );
}
