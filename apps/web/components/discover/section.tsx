import Link from "next/link";
import type { ReactNode } from "react";
import { Unavailable } from "@/components/error-state";
import { Panel } from "@/components/ui";
import { BeeError } from "@/lib/api/transport";

export type Loaded<T> = { ok: true; data: T } | { ok: false; error: BeeError };

/**
 * Runs one read for a section. A server error becomes that section's state (the page still
 * renders the rest); anything that is not a server error is a bug and propagates.
 */
export async function attempt<T>(read: Promise<T>): Promise<Loaded<T>> {
  try {
    return { ok: true, data: await read };
  } catch (err) {
    if (err instanceof BeeError) return { ok: false, error: err };
    throw err;
  }
}

/** The server's reason, shown in place of a section that could not load. Never invented data. */
export function SectionError({ what, error }: { what: string; error: BeeError }) {
  if (error.code === "unimplemented") return <Unavailable what={what} message={error.message} reason={error.reason} />;
  return (
    <div role="alert" className="py-6 text-base text-ink-2">
      <p className="font-medium text-ink">{what} could not be loaded</p>
      <p className="mt-1 [overflow-wrap:anywhere]">{error.message}</p>
      <p className="mt-1 text-sm text-ink-3">reason: {error.reason}</p>
    </div>
  );
}

/** A titled panel with an optional "See all" link in its header. */
export function Section({ id, title, seeAll, children }: { id: string; title: string; seeAll?: { href: string; label: string }; children: ReactNode }) {
  return (
    <Panel
      id={id}
      title={title}
      actions={
        seeAll ? (
          <Link href={seeAll.href} className="link text-sm">
            {seeAll.label}
          </Link>
        ) : undefined
      }
    >
      {children}
    </Panel>
  );
}
