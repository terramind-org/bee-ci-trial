"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { askPerson, Button, ConfirmButton } from "@/components/kit";
import { useToast } from "@/components/toast";
import { browserClient } from "@/lib/api/browser";

export function RunActions({ repo, runId, hasFailed, active }: { repo: string; runId: string; hasFailed: boolean; active: boolean }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const request = { repo: { fullName: repo }, id: runId };

  const rerun = async (failedOnly: boolean) => {
    setBusy(true);
    try {
      const run = await browserClient().workflow.rerun({ ...request, failedOnly });
      toast.push({ tone: "ok", title: "Attempt " + run.attempt + " started" });
      router.refresh();
    } catch (err) {
      toast.error(err, "Could not re-run");
    } finally {
      setBusy(false);
    }
  };

  const cancel = async () => {
    setBusy(true);
    try {
      if (!await askPerson({ title: "Cancel this run?", description: "This stops the jobs that have yet to finish.", confirmLabel: "Cancel run", cancelLabel: "Keep run", destructive: true })) return;
      await browserClient().workflow.cancelRun(request);
      router.refresh();
    } catch (err) {
      toast.error(err, "Could not cancel the run");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-wrap items-center gap-2">
      {active ? (
        <ConfirmButton agentConfirm="Cancels this run and stops its unfinished jobs." consequence="destructive" size="sm" disabled={busy} onClick={cancel}>
          Cancel
        </ConfirmButton>
      ) : (
        <>
          {hasFailed && <Button size="sm" variant="primary" disabled={busy} onClick={() => rerun(true)}>Re-run failed jobs</Button>}
          <Button size="sm" disabled={busy} onClick={() => rerun(false)}>Re-run all</Button>
        </>
      )}
    </div>
  );
}
