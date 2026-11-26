"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { askPerson, ConfirmButton } from "@/components/kit/confirm";
import { browserClient } from "@/lib/api/browser";
import { BeeError } from "@/lib/api/transport";
import type { Visibility } from "@/lib/api/types";
import { VISIBILITIES, VisibilityStatus } from "./visibility-info";

const EDITABLE = ["VISIBILITY_PUBLIC", "VISIBILITY_PRIVATE"] as const;

type VisibilityChoice = (typeof EDITABLE)[number];

export function VisibilitySettings({ repo, visibility, version }: { repo: string; visibility: Visibility; version: string }) {
  const router = useRouter();
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const options = EDITABLE.filter((target) => target !== visibility);

  async function changeVisibility(target: VisibilityChoice) {
    setBusy(true);
    const choice = VISIBILITIES[target];
    const label = "Make " + choice.label.toLowerCase();
    try {
      const confirmed = await askPerson({ title: label + "?", description: choice.description, confirmLabel: label });
      if (!confirmed) return;
      setError("");
      await browserClient().repo.updateRepo({ repo: { fullName: repo }, visibility: target, expectedVersion: version });
      router.refresh();
    } catch (err) {
      setError(err instanceof BeeError ? err.reason + " · " + err.message : "Unable to update visibility.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-4">
      <VisibilityStatus visibility={visibility} />
      <ul className="divide-y divide-line">
        {[...EDITABLE, ...(visibility === "VISIBILITY_INTERNAL" ? ["VISIBILITY_INTERNAL" as const] : [])].map((id) => (
          <li key={id} className="py-3"><strong className="font-medium">{VISIBILITIES[id].label}.</strong> {VISIBILITIES[id].description}</li>
        ))}
      </ul>
      <div className="flex flex-wrap gap-2">
        {options.map((target) => {
          const choice = VISIBILITIES[target];
          const word = choice.label.toLowerCase();
          return (
            <ConfirmButton key={target} consequence="approving" variant={options.length === 1 ? "primary" : "secondary"} disabled={busy}
              agentConfirm={"Makes " + repo + " " + word + ". " + choice.description} onClick={() => changeVisibility(target)}>
              Make {word}
            </ConfirmButton>
          );
        })}
      </div>
      {error ? <p role="alert" className="text-sm text-bad">{error}</p> : null}
    </div>
  );
}
