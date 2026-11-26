"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { askPerson, ConfirmButton, Field, Input, Select } from "@/components/kit";
import { State } from "@/components/parts";
import { browserClient } from "@/lib/api/browser";
import { BeeError } from "@/lib/api/transport";
import type { BillingCycle, BillingPlan } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { billingPrice } from "./billing-format";

type PlanChoice = Pick<BillingPlan, "id" | "name" | "seatMonthlyPriceMicroUsd" | "seatYearlyPriceMicroUsd">;

export function ChangePlan({ org, plans, currentPlan, currentCycle, currentSeats }: {
  org: string;
  plans: PlanChoice[];
  currentPlan: string;
  currentCycle: BillingCycle;
  currentSeats: number;
}) {
  const router = useRouter();
  const [planId, setPlanId] = useState(plans.some((plan) => plan.id === currentPlan) ? currentPlan : "");
  const [cycle, setCycle] = useState<BillingCycle>(currentCycle === "BILLING_CYCLE_YEARLY" ? "BILLING_CYCLE_YEARLY" : "BILLING_CYCLE_MONTHLY");
  const [seats, setSeats] = useState(String(currentSeats));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [done, setDone] = useState("");

  const chosen = plans.find((plan) => plan.id === planId);
  const count = Number(seats);
  const valid = seats !== "" && Number.isInteger(count) && count >= 0 && count <= 0xffff_ffff;
  const yearly = cycle === "BILLING_CYCLE_YEARLY";
  const unitPrice = yearly ? chosen?.seatYearlyPriceMicroUsd : chosen?.seatMonthlyPriceMicroUsd;
  const seatPrice = unitPrice === undefined ? "" : billingPrice(unitPrice) + " per seat per " + (yearly ? "year" : "month") + ".";

  async function submit() {
    if (!chosen || !valid || busy) return;
    setBusy(true);
    setError("");
    setDone("");
    try {
      const confirmed = await askPerson({
        title: "Change the plan for " + org + "?",
        description: [
          chosen.name + ", " + (count === 0 ? "keeping the current seats" : plural(count, "seat")) + ", billed " + (yearly ? "yearly." : "monthly."),
          seatPrice,
          "Plan upgrades take effect now. Plan downgrades take effect at the end of the billing cycle.",
        ].filter(Boolean).join(" "),
        confirmLabel: "Confirm change",
      });
      if (!confirmed) return;
      const result = await browserClient().billing.changePlan({ org, planId, seats: count, cycle });
      setDone(result.pendingPlanId ? "Change scheduled for the end of this billing cycle." : "Plan saved.");
      router.refresh();
    } catch (err) {
      setError(err instanceof BeeError ? err.reason + ": " + err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="flex min-w-0 flex-col gap-4" onSubmit={(event) => {
      event.preventDefault();
      void submit();
    }}>
      <fieldset disabled={busy} className="grid min-w-0 gap-3 sm:grid-cols-2 xl:grid-cols-3">
        <Field label="Plan" htmlFor="plan-id">
          <Select id="plan-id" value={planId} required onChange={(event) => setPlanId(event.target.value)}>
            <option value="" disabled>Choose a plan</option>
            {plans.map((plan) => <option key={plan.id} value={plan.id}>{plan.name}</option>)}
          </Select>
        </Field>
        <Field label="Billing cycle" htmlFor="plan-cycle">
          <Select id="plan-cycle" value={cycle} onChange={(event) => setCycle(event.target.value === "BILLING_CYCLE_YEARLY" ? "BILLING_CYCLE_YEARLY" : "BILLING_CYCLE_MONTHLY")}>
            <option value="BILLING_CYCLE_MONTHLY">Monthly</option>
            <option value="BILLING_CYCLE_YEARLY">Yearly</option>
          </Select>
        </Field>
        <Field label="Seats" htmlFor="plan-seats" hint="0 keeps the current number of seats.">
          <Input id="plan-seats" type="number" min={0} max={0xffff_ffff} step={1} required inputMode="numeric" value={seats} onChange={(event) => setSeats(event.target.value)} className="num" />
        </Field>
      </fieldset>
      {seatPrice ? <p className="num text-sm text-ink-2">{seatPrice}</p> : null}
      {error ? <p role="alert" className="text-sm [overflow-wrap:anywhere]"><State shape="failed">{error}</State></p> : null}
      {done ? <p role="status" className="text-sm"><State shape="passed">{done}</State></p> : null}
      <div>
        <ConfirmButton
          type="submit"
          variant="secondary"
          disabled={!chosen || !valid || busy}
          consequence="approving"
          agentConfirm={"Changes the billing plan, seats or cycle for " + org + "."}
        >
          Review change
        </ConfirmButton>
      </div>
    </form>
  );
}
