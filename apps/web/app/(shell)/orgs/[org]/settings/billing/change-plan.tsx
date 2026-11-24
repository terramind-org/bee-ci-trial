"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { CopyAs } from "@/components/copy-as";
import { Button, Field, Input, Select } from "@/components/ui";
import { browserClient } from "@/lib/api/browser";
import { BeeError } from "@/lib/api/transport";
import type { BillingCycle } from "@/lib/api/types";
import type { Op } from "@/lib/copyas";
import { plural, usd } from "@/lib/format";

export interface PlanChoice {
  id: string;
  name: string;
  monthlyMicroUsd: number;
  yearlyMicroUsd: number;
}

/** Change the plan, seats or cycle. Nothing is sent until the summary is confirmed; the server's refusal is shown as it came. */
export function ChangePlan({ org, plans, currentPlan, currentCycle, currentSeats }: { org: string; plans: PlanChoice[]; currentPlan: string; currentCycle: BillingCycle; currentSeats: number }) {
  const router = useRouter();
  const [planId, setPlanId] = useState(currentPlan);
  const [cycle, setCycle] = useState<BillingCycle>(currentCycle === "BILLING_CYCLE_YEARLY" ? "BILLING_CYCLE_YEARLY" : "BILLING_CYCLE_MONTHLY");
  const [seats, setSeats] = useState(String(currentSeats));
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [done, setDone] = useState("");

  const chosen = plans.find((p) => p.id === planId);
  const n = Number.parseInt(seats, 10);
  const valid = !!chosen && Number.isInteger(n) && n >= 0;
  const req = { org, planId, seats: valid ? n : 0, cycle };
  const op: Op = { service: "BillingService", method: "ChangePlan", request: req, label: "Change the plan" };
  const unit = cycle === "BILLING_CYCLE_YEARLY" ? chosen?.yearlyMicroUsd : chosen?.monthlyMicroUsd;
  const per = cycle === "BILLING_CYCLE_YEARLY" ? "year" : "month";

  async function submit() {
    setBusy(true);
    setError("");
    try {
      const res = await browserClient().billing.changePlan(req);
      setDone(res.pendingPlanId ? "Saved. The change takes effect at the end of this billing cycle." : "Saved. Your plan is now " + (res.plan?.name ?? planId) + ".");
      setConfirming(false);
      router.refresh();
    } catch (e) {
      setError(e instanceof BeeError ? e.reason + ": " + e.message : String(e));
      setConfirming(false);
    } finally {
      setBusy(false);
    }
  }

  return (
    <form
      className="flex flex-col gap-4 px-3.5 py-3.5"
      onSubmit={(e) => {
        e.preventDefault();
        setDone("");
        setError("");
        if (valid) setConfirming(true);
      }}
    >
      <div className="grid gap-3 sm:grid-cols-3">
        <Field label="Plan" htmlFor="plan-id">
          <Select id="plan-id" value={planId} onChange={(e) => setPlanId(e.target.value)}>
            {plans.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Billing cycle" htmlFor="plan-cycle">
          <Select id="plan-cycle" value={cycle} onChange={(e) => setCycle(e.target.value as BillingCycle)}>
            <option value="BILLING_CYCLE_MONTHLY">Monthly</option>
            <option value="BILLING_CYCLE_YEARLY">Yearly</option>
          </Select>
        </Field>
        <Field label="Seats" htmlFor="plan-seats" hint="0 keeps the current number.">
          <Input id="plan-seats" type="number" min={0} inputMode="numeric" value={seats} onChange={(e) => setSeats(e.target.value)} className="num" />
        </Field>
      </div>
      {confirming && chosen ? (
        <div role="alertdialog" aria-labelledby="confirm-title" className="rounded-md border border-line-strong bg-sunken p-3.5" data-testid="confirm-plan">
          <p id="confirm-title" className="text-sm font-medium">
            Switch {org} to {chosen.name}, {n === 0 ? "keeping the current seats" : plural(n, "seat")}, billed {cycle === "BILLING_CYCLE_YEARLY" ? "yearly" : "monthly"}?
          </p>
          <p className="mt-1 text-sm text-ink-2">
            {unit && unit > 0 ? usd(unit) + " per seat per " + per + ". " : ""}A downgrade takes effect at the end of the billing cycle; an upgrade takes effect now.
          </p>
          <div className="mt-3 flex gap-2">
            <Button type="button" variant="primary" disabled={busy} onClick={() => void submit()}>
              {busy ? "Saving…" : "Confirm change"}
            </Button>
            <Button type="button" variant="ghost" disabled={busy} onClick={() => setConfirming(false)}>
              Cancel
            </Button>
          </div>
        </div>
      ) : null}
      {error ? (
        <p role="alert" className="text-sm text-bad [overflow-wrap:anywhere]">
          {error}
        </p>
      ) : null}
      {done ? (
        <p role="status" className="text-sm text-ok">
          {done}
        </p>
      ) : null}
      <div className="flex flex-wrap items-center gap-2">
        <Button type="submit" disabled={!valid || busy || confirming}>
          Review change
        </Button>
        <CopyAs op={op} />
      </div>
    </form>
  );
}
