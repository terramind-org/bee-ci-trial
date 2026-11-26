"use client";

import { useRouter } from "next/navigation";
import { type FormEvent, useId, useState } from "react";
import { askPerson, Button, ConfirmButton, Field, Input, Select, Textarea } from "@/components/kit";
import { useToast } from "@/components/toast";
import { browserClient } from "@/lib/api/browser";
import type { SwarmRef, SwarmState } from "@/lib/api/types";

export interface JoinAgent {
  id: string;
  label: string;
}

/** Convert the entered decimal directly to the wire's integer microdollars. */
export function spendingLimit(input: string): string | undefined {
  const value = input.trim();
  if (!/^(?:[0-9]{1,14}(?:\.[0-9]{0,6})?|\.[0-9]{1,6})$/.test(value)) return undefined;
  const [dollars, fraction = ""] = value.split(".");
  const amount = BigInt(dollars || "0") * 1_000_000n + BigInt(fraction.padEnd(6, "0"));
  if (amount > 0n && amount <= 2n ** 64n - 1n) return String(amount);
}

export function SwarmMessageForm({ swarm }: { swarm: SwarmRef }) {
  const id = useId();
  const router = useRouter();
  const toast = useToast();
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);

  async function send(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!body.trim() || busy) return;
    setBusy(true);
    try {
      await browserClient().swarm.sendSwarmMessage({ swarm, body });
      setBody("");
      toast.push({ tone: "ok", title: "Message sent to the lead" });
      router.refresh();
    } catch (err) {
      toast.error(err, "Message not sent");
    } finally {
      setBusy(false);
    }
  }

  return (
    <form aria-label="Speak to the lead" onSubmit={send} className="flex max-w-xl flex-col gap-3">
      <Field label="Message" hint="Your message goes to the lead only." htmlFor={id}>
        <Textarea id={id} value={body} onChange={(event) => setBody(event.target.value)} maxLength={65_536} />
      </Field>
      <div><Button type="submit" size="sm" variant="primary" disabled={busy || !body.trim()}>{busy ? "Sending…" : "Send message"}</Button></div>
    </form>
  );
}

export function SwarmJoinForm({ swarm, agents, allowAgentId }: { swarm: SwarmRef; agents: JoinAgent[]; allowAgentId: boolean }) {
  const id = useId();
  const router = useRouter();
  const toast = useToast();
  const [agent, setAgent] = useState("");
  const [budget, setBudget] = useState("");
  const [budgetError, setBudgetError] = useState("");
  const [busy, setBusy] = useState(false);

  async function join(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const agentPrincipalId = agent.trim();
    if (!agentPrincipalId || busy) return;
    const maxUsdMicros = spendingLimit(budget);
    if (!maxUsdMicros) {
      setBudgetError("Enter a positive amount in USD, with up to six decimal places.");
      return;
    }
    setBudgetError("");
    setBusy(true);
    try {
      await browserClient().swarm.requestSwarmJoin({ swarm, agentPrincipalId, budget: { maxUsdMicros } });
      setAgent("");
      setBudget("");
      toast.push({ tone: "ok", title: "Join request sent" });
      router.refresh();
    } catch (err) {
      toast.error(err, "Join request not sent");
    } finally {
      setBusy(false);
    }
  }

  return (
    <form aria-label="Join this swarm" onSubmit={join} className="flex max-w-sm flex-col gap-3">
      {allowAgentId ? (
        <Field label="Your agent’s principal ID" hint="Enter the ID of an agent you own." htmlFor={id + "-agent"}>
          <Input id={id + "-agent"} value={agent} onChange={(event) => setAgent(event.target.value)} required />
        </Field>
      ) : (
        <Field label="Your agent" htmlFor={id + "-agent"}>
          <Select id={id + "-agent"} value={agent} onChange={(event) => setAgent(event.target.value)} required>
            <option value="">Choose an agent</option>
            {agents.map((option) => <option key={option.id} value={option.id}>{option.label}</option>)}
          </Select>
        </Field>
      )}
      <Field label="Spending limit (USD)" htmlFor={id + "-budget"} error={budgetError || undefined}>
        <Input id={id + "-budget"} inputMode="decimal" value={budget} onChange={(event) => { setBudget(event.target.value); setBudgetError(""); }} required />
      </Field>
      <div><Button type="submit" size="sm" disabled={busy || !agent.trim() || !budget.trim()}>{busy ? "Sending…" : "Request to join"}</Button></div>
    </form>
  );
}

export function SwarmJoinDecision({ swarm, joinId, applicant }: { swarm: SwarmRef; joinId: string; applicant: string }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [decided, setDecided] = useState(false);

  async function decide(accept: boolean) {
    if (busy || decided) return;
    const label = accept ? "Accept" : "Decline";
    setBusy(true);
    try {
      const confirmed = await askPerson({
        title: label + " the request from " + applicant + "?",
        description: accept ? "If repository rules allow it, this starts the requested agent on the swarm’s branch with the proposed spending limit." : "The requested agent will not join the swarm.",
        confirmLabel: label,
      });
      if (!confirmed) return;
      const result = await browserClient().swarm.decideSwarmJoin({ swarm, joinId, accept });
      setDecided(true);
      if (result.state === "SWARM_JOIN_STATE_REFUSED") {
        toast.push({ tone: "neutral", title: "Repository rules refused the request", detail: result.reason });
      } else {
        toast.push({ tone: "ok", title: accept ? "Join request accepted" : "Join request declined" });
      }
      router.refresh();
    } catch (err) {
      toast.error(err, "Join request not decided");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-wrap gap-2">
      <ConfirmButton size="sm" variant="secondary" disabled={busy || decided} consequence="approving" agentConfirm={"Approves the join request from " + applicant} onClick={() => decide(true)}>Accept</ConfirmButton>
      <ConfirmButton size="sm" variant="secondary" disabled={busy || decided} consequence="destructive" agentConfirm={"Declines the join request from " + applicant} onClick={() => decide(false)}>Decline</ConfirmButton>
    </div>
  );
}

export function SwarmStateControls({ swarm, state }: { swarm: SwarmRef; state: SwarmState }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);

  async function setState(next: SwarmState) {
    if (busy) return;
    setBusy(true);
    try {
      if (next === "SWARM_STATE_STOPPED" && !(await askPerson({
        title: "Stop this swarm?",
        description: "This cancels its active runs and closes pending join requests. Stopped work cannot resume.",
        confirmLabel: "Stop swarm",
        destructive: true,
      }))) return;
      await browserClient().swarm.setSwarmState({ swarm, state: next });
      toast.push({
        tone: "ok",
        title: next === "SWARM_STATE_PAUSED" ? "Swarm paused" : next === "SWARM_STATE_STOPPED" ? "Swarm stopped" : "Swarm resumed",
      });
      router.refresh();
    } catch (err) {
      toast.error(err, "Swarm state not changed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      {state === "SWARM_STATE_WORKING" ? <Button size="sm" disabled={busy} onClick={() => setState("SWARM_STATE_PAUSED")}>Pause</Button> : null}
      {state === "SWARM_STATE_PAUSED" ? <Button size="sm" disabled={busy} onClick={() => setState("SWARM_STATE_WORKING")}>Resume</Button> : null}
      <ConfirmButton size="sm" disabled={busy} consequence="destructive" agentConfirm="Stops the swarm and cancels its active runs" onClick={() => setState("SWARM_STATE_STOPPED")}>Stop</ConfirmButton>
    </>
  );
}
