"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { CopyAs } from "@/components/copy-as";
import { useToast } from "@/components/toast";
import { Button, Field, Input } from "@/components/ui";
import { browserClient } from "@/lib/api/browser";
import type { SetAgentFeedTriggerRequest } from "@/lib/api/types";

export interface FeedTriggerSettings {
  enabled: boolean;
  /** owner/name of each followed source. */
  sources: string[];
  dailyUsdBudget: number;
  maxRunsPerDay: number;
}

/**
 * The feed trigger's settings: on or off, the repos it follows, and the daily budget and run cap.
 * Saving is `AgentService/SetAgentFeedTrigger`; the server decides who may (maintainers), and its
 * reason is shown when it says no. Copy as gives the same call for a script or an agent.
 */
export function FeedTriggerForm({ repo, agentId, initial }: { repo: string; agentId: string; initial: FeedTriggerSettings }) {
  const router = useRouter();
  const toast = useToast();
  const [enabled, setEnabled] = useState(initial.enabled);
  const [sources, setSources] = useState(initial.sources.join("\n"));
  const [budget, setBudget] = useState(String(initial.dailyUsdBudget));
  const [runs, setRuns] = useState(String(initial.maxRunsPerDay));
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const list = sources.split(/[\s,]+/).map((s) => s.trim()).filter(Boolean);
  const req: SetAgentFeedTriggerRequest = {
    repo: { fullName: repo },
    agent: agentId,
    enabled,
    dailyUsdBudget: Number(budget) || 0,
    maxRunsPerDay: Math.max(0, Math.floor(Number(runs) || 0)),
    sources: list.map((fullName) => ({ fullName })),
  };

  async function save() {
    setBusy(true);
    setError("");
    try {
      await browserClient().agent.setAgentFeedTrigger(req);
      toast.push({ tone: "ok", title: enabled ? "Feed trigger on" : "Feed trigger saved", detail: list.length ? "Following " + list.join(", ") + "." : "No sources yet." });
      router.refresh();
    } catch (e) {
      setError((e as Error).message);
      toast.error(e, "Feed trigger not saved");
    } finally {
      setBusy(false);
    }
  }

  return (
    <form
      className="grid max-w-2xl gap-4 sm:grid-cols-2"
      aria-label="Feed trigger settings"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <label className="flex items-center gap-2.5 text-md sm:col-span-2">
        <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} className="size-4 accent-[var(--accent)]" />
        React to new posts
      </label>
      <div className="sm:col-span-2">
        <Field label="Sources" htmlFor="trigger-sources" hint="One owner/name per line. The agent follows each one and must be able to read it.">
          <textarea
            id="trigger-sources"
            value={sources}
            onChange={(e) => setSources(e.target.value)}
            rows={3}
            spellCheck={false}
            placeholder="lattice/engine"
            className="mono w-full rounded-md border border-line bg-surface px-3 py-2 text-base text-ink placeholder:text-ink-3 focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent"
          />
        </Field>
      </div>
      <Field label="Daily budget (USD)" htmlFor="trigger-budget" hint="Runs stop for the day once this is spent.">
        <Input id="trigger-budget" type="number" min={0} step="0.01" value={budget} onChange={(e) => setBudget(e.target.value)} />
      </Field>
      <Field label="Runs per day" htmlFor="trigger-runs" hint="Posts that arrive together share one run.">
        <Input id="trigger-runs" type="number" min={0} step={1} value={runs} onChange={(e) => setRuns(e.target.value)} />
      </Field>
      {error ? (
        <p className="text-sm text-bad sm:col-span-2" role="alert">
          {error}
        </p>
      ) : null}
      <div className="flex items-center gap-1.5 sm:col-span-2">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? "Saving…" : "Save feed trigger"}
        </Button>
        <CopyAs op={{ service: "AgentService", method: "SetAgentFeedTrigger", request: req, label: "Set this agent's feed trigger" }} />
      </div>
    </form>
  );
}
