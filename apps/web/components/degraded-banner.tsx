"use client";

import { useEffect, useState } from "react";
import { DEGRADED_EVENT, degradedMessage } from "@/lib/infra";
import { Icon } from "./icons";

/**
 * Shown at the top of a repo while the metadata store is down (SPEC §15.1): reads come from
 * cached rows, so they can be old, and every write fails. `ageSeconds` is the staleness the server
 * reported, or null when it did not say. `degraded` is the server's verdict at render time; a write
 * that fails with the outage reason raises the same banner without a reload.
 */
export function DegradedBanner({ degraded, ageSeconds }: { degraded: boolean; ageSeconds: number | null }) {
  const [raised, setRaised] = useState(false);
  useEffect(() => {
    const on = () => setRaised(true);
    window.addEventListener(DEGRADED_EVENT, on);
    return () => window.removeEventListener(DEGRADED_EVENT, on);
  }, []);
  if (!degraded && !raised) return null;
  return (
    <div role="status" data-testid="degraded-banner" className="mb-5 flex items-start gap-2 rounded-md bg-warn-soft px-3.5 py-2.5 text-sm text-warn">
      <Icon name="alert" size={16} className="mt-0.5 shrink-0" />
      <p>{degradedMessage(degraded ? ageSeconds : null)}</p>
    </div>
  );
}
