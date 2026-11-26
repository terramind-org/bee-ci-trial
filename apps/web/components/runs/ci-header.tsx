import Link from "next/link";
import type { ReactNode } from "react";

/** Keep the CI actions beside the page title even on a narrow screen. */
export function CiHeader({ title, back, actions }: { title: string; back?: string; actions: ReactNode }) {
  return (
    <header className="mb-8 sm:mb-10">
      {back && <Link href={back} className="link mb-2 inline-flex min-h-6 items-center text-base text-ink-2">CI</Link>}
      <div className="flex items-center justify-between gap-3">
        <h1 className="min-w-0 text-2xl text-ink [overflow-wrap:anywhere]">{title}</h1>
        <div className="flex shrink-0 items-center gap-2">{actions}</div>
      </div>
    </header>
  );
}
