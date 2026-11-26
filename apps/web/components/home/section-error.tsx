"use client";

import { useRouter } from "next/navigation";
import { Button } from "@/components/ui";

/** An error inside one section: the server's reason and message, and a retry. The rest of the page stays. */
export function SectionError({ what, reason, message }: { what: string; reason: string; message: string }) {
  const router = useRouter();
  if (reason === "UNIMPLEMENTED") {
    return (
      <div role="status" data-testid="unavailable" className="rounded-lg border border-dashed border-line-strong px-3.5 py-4 text-sm">
        <p className="font-medium">{what} is not available on this server</p>
        <p className="mt-1 text-ink-2 [overflow-wrap:anywhere]">{message}</p>
        <p className="mono mt-1 text-xs text-ink-3">reason: {reason}</p>
      </div>
    );
  }
  return (
    <div role="alert" className="rounded-lg border border-line bg-surface px-3.5 py-4 text-sm">
      <p className="font-medium">{what} could not load</p>
      <p className="mt-1 text-ink-2 [overflow-wrap:anywhere]">{message}</p>
      <p className="mono mt-1 text-xs text-ink-3">reason: {reason}</p>
      <Button size="sm" icon="refresh" className="mt-2.5" onClick={() => router.refresh()}>
        Try again
      </Button>
    </div>
  );
}
