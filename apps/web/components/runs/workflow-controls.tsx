"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { askPerson, Button, ConfirmButton } from "@/components/kit";
import { useToast } from "@/components/toast";
import { browserClient } from "@/lib/api/browser";
import type { WorkflowDefinition } from "@/lib/api/types";

export function WorkflowControls({ repo, workflow }: { repo: string; workflow: WorkflowDefinition }) {
  const router = useRouter();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const request = { repo: { fullName: repo }, workflow: workflow.id };

  const change = async (enable: boolean) => {
    setBusy(true);
    try {
      if (!enable && !await askPerson({
        title: "Disable " + workflow.name + "?",
        description: "New events will stop starting this workflow until it is enabled again.",
        confirmLabel: "Disable workflow",
        destructive: true,
      })) return;
      const api = browserClient().workflow;
      if (enable) await api.enableWorkflow(request);
      else await api.disableWorkflow(request);
      router.refresh();
    } catch (err) {
      toast.error(err, enable ? "Could not enable the workflow" : "Could not disable the workflow");
    } finally {
      setBusy(false);
    }
  };

  const badge = async () => {
    try {
      const { svg } = await browserClient().workflow.getWorkflowBadge(request);
      if (!svg) {
        toast.push({ tone: "neutral", title: "No badge is available for this workflow" });
        return;
      }
      // Download the server's badge as a file without inserting raw SVG into the page.
      const url = URL.createObjectURL(new Blob([svg], { type: "application/octet-stream" }));
      const link = document.createElement("a");
      link.href = url;
      link.download = (workflow.path.split("/").pop()?.replace(/\.ya?ml$/, "") || "workflow") + "-badge.svg";
      link.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (err) {
      toast.error(err, "Could not download the badge");
    }
  };

  return (
    <div className="flex flex-wrap items-center gap-2">
      <Button size="sm" onClick={badge}>Download badge</Button>
      {workflow.state === "WORKFLOW_STATE_ACTIVE" && (
        <ConfirmButton agentConfirm="Disables this workflow so new events no longer start it." consequence="destructive" size="sm" onClick={() => change(false)} disabled={busy}>
          Disable workflow
        </ConfirmButton>
      )}
      {workflow.state?.startsWith("WORKFLOW_STATE_DISABLED_") && <Button size="sm" onClick={() => change(true)} disabled={busy}>Enable workflow</Button>}
    </div>
  );
}
