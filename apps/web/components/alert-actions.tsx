"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { CopyAs } from "./copy-as";
import { ConfirmButton } from "./kit/confirm";
import { useToast } from "./toast";
import { Select } from "./ui";

const REASONS = ["False positive", "Used in tests only", "Risk accepted", "No bandwidth to fix"];

export function DismissAlert({ repo, id }: { repo: string; id: string }) {
  const router = useRouter();
  const toast = useToast();
  const [reason, setReason] = useState(REASONS[0] ?? "");
  const req = { repo: { fullName: repo }, id, reason };
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <label htmlFor={"dismiss-" + id} className="sr-only">
        Reason
      </label>
      <Select id={"dismiss-" + id} value={reason} onChange={(e) => setReason(e.target.value)} className="h-7 w-44 text-sm">
        {REASONS.map((r) => (
          <option key={r}>{r}</option>
        ))}
      </Select>
      <ConfirmButton
        agentConfirm={"Dismisses this security alert in " + repo + " with the reason chosen beside it; the reason goes in the audit log"}
        consequence="destructive"
        variant="secondary"
        size="sm"
        onClick={async () => {
          try {
            await browserClient().security.dismissAlert(req);
            toast.push({ tone: "ok", title: "Alert dismissed", detail: "The reason is in the audit log." });
            router.refresh();
          } catch (e) {
            toast.error(e);
          }
        }}
      >
        Dismiss
      </ConfirmButton>
      <CopyAs compact op={{ service: "SecurityService", method: "DismissAlert", request: req, label: "Dismiss this alert" }} />
    </div>
  );
}
