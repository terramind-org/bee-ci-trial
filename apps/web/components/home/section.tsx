/**
 * Section frames for the home page. Every section loads on its own and fails on its own: the
 * server's reason is shown in place, UNIMPLEMENTED reads as "not available on this server", and
 * nothing is invented. (ErrorState is page-sized, so sections use SectionError.)
 */
import type { ReactNode } from "react";
import { BeeError } from "@/lib/api/transport";
import { SectionError } from "./section-error";
import { Panel, Skeleton } from "@/components/ui";

/** Loads a section, then renders it; a BeeError becomes an honest in-place error. */
export async function loadSection<T>(what: string, load: () => Promise<T>, render: (data: T) => ReactNode): Promise<ReactNode> {
  try {
    return render(await load());
  } catch (err) {
    if (err instanceof BeeError) return <SectionError what={what} reason={err.reason} message={err.message} />;
    throw err;
  }
}

/** A panel with its real title and `rows` placeholder rows of `rowHeight` px, for Suspense fallbacks. */
export function PanelFallback({ title, rows, rowHeight, actions = false }: { title: string; rows: number; rowHeight: number; actions?: boolean }) {
  return (
    <Panel title={title} actions={actions ? <span className="block h-7 w-12" /> : undefined}>
      <ul className="divide-y divide-line" aria-busy="true" aria-label={"Loading " + title.toLowerCase()}>
        {Array.from({ length: rows }, (_, i) => (
          <li key={i} className="flex items-center gap-2.5 px-3.5" style={{ height: rowHeight }}>
            <Skeleton className="size-5 shrink-0 rounded-full" />
            <Skeleton className="h-2.5 flex-1" />
            <Skeleton className="h-2.5 w-8" />
          </li>
        ))}
      </ul>
    </Panel>
  );
}
