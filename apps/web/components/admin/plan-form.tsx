"use client";

import { useState } from "react";
import { CopyAs } from "../copy-as";
import { Field, Select } from "../ui";
import { ActionError, ConfirmButton, actionsRow, formClass, useAdminAction } from "./kit";

/** The programs only a site administrator can grant (the server refuses everyone else). */
const PROGRAMS = [
  { id: "education", label: "Education: Team at a 100% discount" },
  { id: "nonprofit", label: "Nonprofit: Team at a 50% discount" },
] as const;

/** Puts an organization on the education or nonprofit plan. */
export function PlanForm({ orgs, current }: { orgs: string[]; current: Record<string, string> }) {
  const [org, setOrg] = useState(orgs[0] ?? "");
  const [planId, setPlanId] = useState<string>(PROGRAMS[0].id);
  const { run, busy, error } = useAdminAction();
  const request = { org, planId };
  const program = PROGRAMS.find((p) => p.id === planId);

  return (
    <form className={formClass} onSubmit={(e) => e.preventDefault()}>
      {/* One column: plan names are long, and a half-width select cuts them off. */}
      <div className="grid grid-cols-1 gap-3.5">
        <Field label="Organization" htmlFor="plan-org" hint={current[org] ? "Currently on the " + current[org] + " plan." : undefined}>
          <Select id="plan-org" value={org} onChange={(e) => setOrg(e.target.value)}>
            {orgs.map((o) => (
              <option key={o}>{o}</option>
            ))}
          </Select>
        </Field>
        <Field label="Plan" htmlFor="plan-id">
          <Select id="plan-id" value={planId} onChange={(e) => setPlanId(e.target.value)}>
            {PROGRAMS.map((p) => (
              <option key={p.id} value={p.id}>
                {p.label}
              </option>
            ))}
          </Select>
        </Field>
      </div>
      <p className="text-xs text-ink-3">Only a site administrator can do this. Check that the organization is eligible first: Beehive does not.</p>
      <ActionError error={error} />
      <div className={actionsRow}>
        <ConfirmButton
          size="md"
          variant="primary"
          label="Assign plan"
          title={"Put " + org + " on the " + (program?.id ?? planId) + " plan?"}
          description={"This changes what " + org + " is billed from now on. You can move it to another plan afterwards."}
          confirmLabel="Assign plan"
          disabled={busy || !org}
          onConfirm={async () => void (await run("changePlan", request, org + " is on the " + planId + " plan"))}
        />
        <CopyAs op={{ service: "BillingService", method: "ChangePlan", request, label: "Assign a plan" }} />
      </div>
    </form>
  );
}
