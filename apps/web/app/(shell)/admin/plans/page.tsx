import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { PlanForm } from "@/components/admin/plan-form";
import { EmptyState, Panel } from "@/components/ui";
import { Failed, serverClient, settle } from "../load";

export const metadata: Metadata = { title: "Admin · Plans" };
export const dynamic = "force-dynamic";

/** Grant the education or nonprofit plan to an organization. */
export default async function PlansPage() {
  const api = await serverClient();
  const orgs = await settle(api.org.listOrgs({}));
  if (!orgs.ok) return <Failed what="The organization list" error={orgs.error} />;
  const names = orgs.value.orgs.map((o) => o.name);
  const plans = await Promise.all(names.map(async (org) => [org, await settle(api.billing.getAccountPlan({ org }))] as const));
  const current = Object.fromEntries(plans.flatMap(([org, p]) => (p.ok && p.value.plan ? [[org, p.value.plan.name || p.value.plan.id] as const] : [])));
  const op = { service: "BillingService", method: "GetAccountPlan", request: { org: names[0] ?? "" }, label: "Show an organization's plan" } as const;
  return (
    <>
      {names.length ? <PageOp op={op} /> : null}
      <div className="grid grid-cols-1 gap-10 xl:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <Panel title="Assign a plan" id="plan-assign">
          {names.length ? <PlanForm orgs={names} current={current} /> : <EmptyState title="No organizations" icon="team">Create an organization first.</EmptyState>}
        </Panel>
        <Panel title="Current plans" id="plan-current" actions={names.length ? <CopyAs compact op={op} /> : null}>
          {names.length ? (
            <ul aria-label="Current plans" className="divide-y divide-line">
              {plans.map(([org, p]) => (
                <li key={org} className="flex items-center gap-3 py-3 text-base">
                  <span className="font-medium">{org}</span>
                  <span className="ml-auto text-ink-2">{p.ok ? (p.value.plan?.name || p.value.plan?.id || "No plan") : p.error.code === "unimplemented" ? "Not available on this server" : p.error.reason}</span>
                </li>
              ))}
            </ul>
          ) : (
            <EmptyState title="No organizations" icon="team" />
          )}
        </Panel>
      </div>
    </>
  );
}
