"use client";

import { useRouter } from "next/navigation";
import { useState, type FormEvent } from "react";
import { askPerson, Button, ConfirmButton, Field, Input } from "@/components/kit";
import { useToast } from "@/components/toast";
import { browserClient } from "@/lib/api/browser";
import type { AgentOwnerBudget } from "@/lib/api/types";
import { asCount, asMicros, BUDGET_LIMITS, dollars } from "./agents-budget";

export function OrgRevokeToken({ tokenId, name }: { tokenId: string; name: string }) {
  const router = useRouter();
  const toast = useToast();
  return (
    <ConfirmButton
      agentConfirm={"Revokes " + name + " and tokens derived from it"}
      aria-label={"Revoke " + name}
      consequence="destructive"
      size="sm"
      onClick={async () => {
        if (!(await askPerson({ title: "Revoke " + name + "?", description: "Tokens derived from it will also stop working. This cannot be undone.", confirmLabel: "Revoke token", destructive: true }))) return;
        try {
          await browserClient().auth.revoke({ tokenId });
          toast.push({ tone: "ok", title: "Token revoked" });
          router.refresh();
        } catch (err) {
          toast.error(err, "Could not revoke token");
        }
      }}
    >
      Revoke
    </ConfirmButton>
  );
}

export function OwnerBudgetControl({ org, ownerId, ownerName, budget }: { org: string; ownerId: string; ownerName: string; budget: AgentOwnerBudget }) {
  const router = useRouter();
  const toast = useToast();
  const [editing, setEditing] = useState(false);
  const [pending, setPending] = useState(false);
  const [invalid, setInvalid] = useState<(typeof BUDGET_LIMITS)[number]["field"] | null>(null);

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (pending) return;
    const data = new FormData(event.currentTarget);
    const request = { org, ownerId, maxUsdMicros: "0", maxInputTokens: "0", maxOutputTokens: "0", maxRuns: "0" };
    for (const { field } of BUDGET_LIMITS) {
      const value = String(data.get(field) ?? "");
      const encoded = field === "maxUsdMicros" ? asMicros(value) : asCount(value);
      if (encoded === null) {
        setInvalid(field);
        return;
      }
      request[field] = encoded;
    }
    setInvalid(null);
    setPending(true);
    try {
      const agreed = await askPerson({
        title: "Set the budget for " + ownerName + "’s agents?",
        description: "These monthly limits apply to all agents that draw on this person’s budget in " + org + ".",
        confirmLabel: "Set budget",
      });
      if (!agreed) return;
      await browserClient().agent.setAgentBudget(request);
      toast.push({ tone: "ok", title: "Agent budget updated" });
      setEditing(false);
      router.refresh();
    } catch (err) {
      toast.error(err, "Could not set the agent budget");
    } finally {
      setPending(false);
    }
  }

  if (!editing) return <Button size="sm" aria-label={"Edit budget for " + ownerName} onClick={() => setEditing(true)}>Edit budget</Button>;
  return (
    <form onSubmit={save} aria-label={"Budget for " + ownerName} className="mt-4 grid max-w-xl gap-3 sm:grid-cols-2">
      <p className="text-sm text-ink-2 sm:col-span-2">Enter 0 for an unlimited amount.</p>
      {BUDGET_LIMITS.map(({ field, label }) => {
        const id = ownerId + "-" + field;
        const amount = budget[field] || "0";
        return (
          <Field key={field} label={label} htmlFor={id} error={invalid === field ? "Enter a supported non-negative amount." : undefined}>
            <Input
              id={id}
              name={field}
              className="num"
              defaultValue={field === "maxUsdMicros" ? dollars(amount) : amount}
              inputMode={field === "maxUsdMicros" ? "decimal" : "numeric"}
              disabled={pending}
              required
            />
          </Field>
        );
      })}
      <div className="flex flex-wrap gap-2 sm:col-span-2">
        <ConfirmButton
          type="submit"
          size="sm"
          variant="secondary"
          consequence="approving"
          agentConfirm={"Sets the monthly limits for " + ownerName + "’s agents in " + org}
          disabled={pending}
        >
          {pending ? "Setting budget…" : "Set budget"}
        </ConfirmButton>
        <Button type="button" size="sm" variant="ghost" disabled={pending} onClick={() => { setInvalid(null); setEditing(false); }}>Cancel</Button>
      </div>
    </form>
  );
}
