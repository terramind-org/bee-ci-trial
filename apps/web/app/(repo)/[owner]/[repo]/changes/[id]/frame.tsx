"use client";

/**
 * The pull request page's two shapes. The overview is one reading column: everything a reviewer
 * needs is in the page, in order. Files changed, Checks and For agents keep the side rail (the
 * rules, the stack and the revisions) beside their content, under it below 1024px. The shape
 * follows the layout segment, so the layout stays a server component.
 */
import { useSelectedLayoutSegment } from "next/navigation";
import type { ReactNode } from "react";
import { tabOf } from "@/lib/change-page";

export function ChangeFrame({ rail, children }: { rail: ReactNode; children: ReactNode }) {
  const overview = tabOf(useSelectedLayoutSegment()) === "brief";
  if (overview) return <div className="min-w-0">{children}</div>;
  return (
    <div className="grid grid-cols-1 gap-x-14 gap-y-10 lg:grid-cols-[minmax(0,1fr)_300px]">
      {/* The tab first, so on a phone each tab opens on its own content. */}
      <div className="min-w-0">{children}</div>
      <aside className="flex min-w-0 flex-col gap-8" aria-label="Rules, stack and revisions">
        {rail}
      </aside>
    </div>
  );
}
