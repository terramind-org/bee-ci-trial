import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { EmptyState } from "@/components/kit";
import { PRODUCT_LABEL } from "@/components/marketing/pricing";
import { PageMenu } from "@/components/menu";
import { billingPrice } from "@/components/org/billing-format";
import { ChangePlan } from "@/components/org/change-plan";
import { Kv, Quiet, Row, Rows, Sect, Sep, State } from "@/components/parts";
import { PageHeader } from "@/components/ui";
import { edition, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { AccountPlan, BillingPlan } from "@/lib/api/types";
import { absoluteTime, plural } from "@/lib/format";
import { guard } from "@/lib/page";
import { toolFor } from "@/lib/tools";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Billing" };
export const dynamic = "force-dynamic";

const GET_PLAN = "BillingService/GetAccountPlan";
const LIST_PLANS = "BillingService/ListBillingPlans";
const QUERY_USAGE = "BillingService/QueryUsage";
const LIST_SEATS = "BillingService/ListSeats";
const CHANGE_PLAN = "BillingService/ChangePlan";

const planTools = [
  toolFor(GET_PLAN, "Read the current plan and billing cycle"),
  toolFor(LIST_PLANS, "List available plans and per-seat prices"),
];

const num = (n: number) => n.toLocaleString("en-US", { maximumFractionDigits: 2 });
const formatError = (err: BeeError) => err.reason + ": " + err.message;

function periodOf(cycle: AccountPlan["cycle"]): string {
  if (cycle === "BILLING_CYCLE_MONTHLY") return "Monthly";
  if (cycle === "BILLING_CYCLE_YEARLY") return "Yearly";
  return "Not set";
}

async function billingSection<T>(read: Promise<T>) {
  try {
    return { data: await read };
  } catch (err) {
    if (err instanceof BeeError) return { error: formatError(err) };
    throw err;
  }
}

export default async function BillingPage({ params }: { params: Promise<{ org: string }> }) {
  if ((await edition()) !== "cloud") notFound();
  return guard(async () => {
    const [api, { org: encodedOrg }, { mode }] = await Promise.all([serverClient(), params, requestMode()]);
    const org = decodeURIComponent(encodedOrg);
    const op = { service: "BillingService", method: "GetAccountPlan", request: { org }, label: "Show the plan" } as const;

    let account: AccountPlan;
    let plans: BillingPlan[];
    try {
      [account, { plans }] = await Promise.all([api.billing.getAccountPlan({ org }), api.billing.listBillingPlans({ scope: { org } })]);
    } catch (err) {
      if (!(err instanceof BeeError) || err.code !== "unimplemented") throw err;
      const empty = "Billing is not available on this server.";
      if (mode === "bee") {
        return <PageFacts page="org-billing" title={org + " · billing"}
          groups={[{ title: "billing", lines: [empty, formatError(err)] }]} tools={planTools} />;
      }
      return <>
        <PageHeader title="Billing" eyebrow={org} />
        <SettingsTabs org={org} active="billing" />
        <div data-testid="billing-unavailable"><EmptyState title={empty}>{formatError(err)}</EmptyState></div>
      </>;
    }

    const [usage, seats] = await Promise.all([
      billingSection(api.billing.queryUsage({ org, startAt: account.cycleStartedAt, endAt: account.nextBillingAt, groupBy: ["BILLING_USAGE_GROUPING_PRODUCT"] })),
      billingSection(api.billing.listSeats({ org })),
    ]);
    const selfServe = plans.filter((plan) => plan.selfServe);
    const canChange = !account.billingDisabled && selfServe.length > 0;
    const planFacts: FactGroup = {
      title: "current plan",
      facts: [
        { label: "Plan", value: account.plan?.name ?? "None" },
        { label: "Billing cycle", value: periodOf(account.cycle) },
        { label: "Seats", value: <span className="num">{account.seatsConsumed} in use of {account.seatsPurchased} purchased</span> },
        account.cycleStartedAt ? { label: "Cycle started", value: <time dateTime={account.cycleStartedAt}>{absoluteTime(account.cycleStartedAt)}</time> } : null,
        { label: "Next bill", value: account.nextBillingAt ? <time dateTime={account.nextBillingAt}>{absoluteTime(account.nextBillingAt)}</time> : "None scheduled" },
      ],
    };
    const pending = plans.find((plan) => plan.id === account.pendingPlanId)?.name ?? account.pendingPlanId;
    const pendingChange = pending ? "At the end of this cycle the account moves to " + pending + " with " + plural(account.pendingSeats, "seat") + "." : "";
    const disabled = "Billing is off for this account.";
    const noUsage = "No usage has been metered this cycle.";
    const noSeats = "No seats are in use.";
    const seatLines = (seats.data?.seats ?? []).map((seat) => ({
      id: seat.principal?.id ?? seat.org,
      name: seat.principal?.display || seat.principal?.id,
      outside: seat.outsideCollaborator,
      activeAt: seat.lastActivityAt,
    })).filter((seat) => seat.name);
    const usageLines = (usage.data?.lineItems ?? []).map((line) => ({
      id: JSON.stringify([line.product, line.sku, line.date]),
      name: PRODUCT_LABEL[line.product] ?? line.product,
      quantity: num(line.quantity) + " " + line.unit,
      charged: billingPrice(line.netMicroUsd),
    }));
    const totalCharge = usage.data && "Total charged " + billingPrice(usage.data.totalNetMicroUsd) + ".";
    const usageThrough = usage.data?.dataCompleteUntil;
    const usageNote = usageThrough ? "Usage is complete through " + absoluteTime(usageThrough) + "." : "";
    const planOptions = selfServe.map(({ id, name, seatMonthlyPriceMicroUsd, seatYearlyPriceMicroUsd }) => ({ id, name, seatMonthlyPriceMicroUsd, seatYearlyPriceMicroUsd }));

    if (mode === "bee") {
      return <PageFacts
        page="org-billing"
        title={org + " · billing"}
        groups={[
          { ...planFacts, lines: [account.billingDisabled ? disabled : "", pendingChange].filter(Boolean) },
          ...(canChange ? [{
            title: "available plans",
            lines: planOptions.map((plan) => plan.name + " · " + billingPrice(plan.seatMonthlyPriceMicroUsd) + " per seat per month · " + billingPrice(plan.seatYearlyPriceMicroUsd) + " per seat per year"),
          }] : []),
          {
            title: "usage this cycle",
            lines: usage.error ? [usage.error] : [
              ...usageLines.map((line) => [line.name, line.quantity, line.charged + " charged"].join(" · ")),
              ...(!usageLines.length ? [noUsage] : []),
              ...[totalCharge, usageNote].filter(Boolean),
            ],
          },
          {
            title: "seats",
            facts: seats.data ? [{ label: "In use", value: seats.data.totalConsumed }] : undefined,
            lines: seats.error ? [seats.error] : seatLines.length ? seatLines.map((seat) => [seat.name, seat.outside && "outside collaborator", seat.activeAt && "last active " + absoluteTime(seat.activeAt)].filter(Boolean).join(" · ")) : [noSeats],
          },
        ]}
        tools={[
          ...planTools,
          toolFor(QUERY_USAGE, "Read metered usage and charges"),
          toolFor(LIST_SEATS, "List people consuming seats"),
          ...(canChange ? [toolFor(CHANGE_PLAN, "Change the plan, billing cycle or seats")] : []),
        ]}
      />;
    }

    return (
      <>
        <PageHeader title="Billing" eyebrow={org} actions={<PageMenu op={op} />} />
        <SettingsTabs org={org} active="billing" />
        <div className="flex max-w-5xl flex-col gap-10">
          <Sect title="Current plan" id="current-plan-title">
            <div data-testid="current-plan"><Kv rows={(planFacts.facts ?? []).map((fact) => fact ? [fact.label, fact.value] : null)} /></div>
            {account.billingDisabled ? <Quiet className="mt-3"><State shape="closed">{disabled}</State></Quiet> : null}
            {pendingChange ? <p className="mt-3 text-base text-ink-2" data-testid="pending-change"><State shape="waiting">{pendingChange}</State></p> : null}
          </Sect>
          {canChange ? (
            <Sect title="Change plan" id="change-plan">
              <ChangePlan org={org} plans={planOptions} currentPlan={account.plan?.id ?? ""} currentCycle={account.cycle} currentSeats={account.seatsPurchased} />
            </Sect>
          ) : null}
          <Sect title="Usage this cycle" id="usage">
            {usage.error ? <Quiet><State shape="failed">{usage.error}</State></Quiet> : <>
              {usageLines.length ? (
                <Rows label="Metered usage since the cycle started, by product" testId="usage-table" className="border-t-0">
                  {usageLines.map((line) => <Row key={line.id} meta={<span className="num">{line.quantity}</span>} action={<span className="num text-base">{line.charged} charged</span>}>{line.name}</Row>)}
                </Rows>
              ) : <Quiet>{noUsage}</Quiet>}
              {totalCharge ? <p className="num mt-3 text-base font-medium">{totalCharge}</p> : null}
              {usageNote ? <Quiet className="mt-1">{usageNote}</Quiet> : null}
            </>}
          </Sect>
          <Sect title="Seats" id="seats">
            {seats.error ? <Quiet><State shape="failed">{seats.error}</State></Quiet> : <>
              {seats.data ? <Quiet className="num">{seats.data.totalConsumed} in use</Quiet> : null}
              {seatLines.length ? (
                <Rows testId="seats" className="mt-3">
                  {seatLines.map((seat) => {
                    const activity = seat.activeAt && <>Last active <time dateTime={seat.activeAt}>{absoluteTime(seat.activeAt)}</time></>;
                    return <Row key={seat.id} meta={(seat.outside || activity) && <>
                      {seat.outside ? <State shape="open">Outside collaborator</State> : null}
                      {seat.outside && activity ? <Sep /> : null}
                      {activity}
                    </>}>{seat.name}</Row>;
                  })}
                </Rows>
              ) : <Quiet>{noSeats}</Quiet>}
            </>}
          </Sect>
        </div>
      </>
    );
  });
}
