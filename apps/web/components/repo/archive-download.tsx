"use client";

import { useState, useTransition } from "react";
import { Icon } from "@/components/icons";
import { Button } from "@/components/ui";
import { requestArchive } from "./archive-actions";

/** The headline for a refused archive, by the server's SPEC §12.2 reason; the server's own text follows it. */
export function archiveRefusal(reason: string): string {
  switch (reason) {
    case "REDACTED":
      return "Refused: this tree contains redacted content";
    case "POLICY_DENIED":
    case "PERMISSION_DENIED":
    case "FORBIDDEN":
      return "You lack read access to some paths";
    case "UNAUTHENTICATED":
      return "Sign in to download a source archive";
    case "UNIMPLEMENTED":
      return "Source archives are not available on this server";
    default:
      return "Could not prepare the archive";
  }
}

/** Source archive buttons (tar.gz and zip). A refusal stays on screen, with the server's reason. */
export function ArchiveDownload({ fullName }: { fullName: string }) {
  const [pending, start] = useTransition();
  const [busy, setBusy] = useState<string | null>(null);
  const [refused, setRefused] = useState<{ headline: string; reason: string; message: string } | null>(null);

  const get = (format: "tar.gz" | "zip") => {
    setBusy(format);
    setRefused(null);
    start(async () => {
      const res = await requestArchive(fullName, format);
      if (res.ok) {
        window.location.assign(res.url);
      } else {
        setRefused({ headline: archiveRefusal(res.reason), reason: res.reason, message: res.message });
      }
      setBusy(null);
    });
  };

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs text-ink-3">Source</span>
        <Button size="sm" onClick={() => get("tar.gz")} disabled={pending} aria-label="Download source as tar.gz" icon="import">
          {busy === "tar.gz" ? "Preparing…" : "tar.gz"}
        </Button>
        <Button size="sm" onClick={() => get("zip")} disabled={pending} aria-label="Download source as zip" icon="import">
          {busy === "zip" ? "Preparing…" : "zip"}
        </Button>
      </div>
      {refused ? (
        <div role="alert" className="flex gap-2 rounded-md border border-line bg-bad-soft px-2.5 py-2 text-sm text-bad">
          <Icon name="alert" className="mt-0.5 shrink-0" />
          <div className="min-w-0">
            <p className="font-medium">{refused.headline}</p>
            <p className="text-xs [overflow-wrap:anywhere]">
              {refused.message} <span className="mono">({refused.reason})</span>
            </p>
          </div>
        </div>
      ) : null}
    </div>
  );
}
